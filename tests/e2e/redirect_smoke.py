import argparse
import concurrent.futures
import contextlib
import http.client
import importlib.metadata
import json
import os
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from yt_dlp.networking.common import _REQUEST_HANDLERS


class Source(BaseHTTPRequestHandler):
    media = b"local media"

    def log_message(self, *_):
        pass

    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        parts = urlsplit(self.path).path.strip("/").split("/")
        kind, count, case = parts
        target = f"/media/0/{case}"
        if kind in ("http", "extract") and int(count) > 1:
            target = f"/{kind}/{int(count) - 1}/{case}"
        if kind in ("http", "loop"):
            self.send_response(302)
            self.send_header("Location", self.path if kind == "loop" else target)
            self.end_headers()
            return
        if kind == "cycle":
            target = f"/cycle/{1 - int(count)}/{case}"
        if kind in ("extract", "cycle"):
            body = f'<html><head><meta http-equiv="refresh" content="0; url={target}"></head></html>'.encode()
            content_type = "text/html"
        else:
            body = self.media
            content_type = "video/mp4"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)


class Proxy(BaseHTTPRequestHandler):
    source_port = 0
    requests = 0

    def log_message(self, *_):
        pass

    def do_GET(self):
        parsed = urlsplit(self.path)
        if parsed.hostname != "redirect.test" or parsed.port != self.source_port:
            self.send_error(400)
            return
        type(self).requests += 1
        connection = http.client.HTTPConnection(
            "127.0.0.1", self.source_port, timeout=5
        )
        try:
            connection.request("GET", parsed.path)
            response = connection.getresponse()
            self.send_response(response.status)
            for name in ("Content-Type", "Content-Length", "Location"):
                value = response.getheader(name)
                if value is not None:
                    self.send_header(name, value)
            self.end_headers()
            self.wfile.write(response.read())
        finally:
            connection.close()


@contextlib.contextmanager
def http_server(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server.server_port
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def api(base, path, body=None):
    request = urllib.request.Request(
        base + path,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)


@contextlib.contextmanager
def aulos(depth, proxy=""):
    with tempfile.TemporaryDirectory(prefix="aulos-redirect-") as temp:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        env = os.environ | {
            "HOST": "127.0.0.1",
            "PORT": str(port),
            "DOWNLOAD_DIR": temp + "/downloads",
            "STATE_DIR": temp + "/state",
            "TEMP_DIR": temp + "/tmp",
            "AULOS_POT_ENABLED": "false",
            "AULOS_AUTO_RETRY_MAX": "0",
            "AULOS_RESOLVE_MAX_DEPTH": str(depth),
            "AULOS_SHUTDOWN_GRACE_SECS": "0",
            "YTDL_OPTIONS": json.dumps({"proxy": proxy, "cachedir": False}),
        }
        log_path = Path(temp) / "server.log"
        with log_path.open("w") as log:
            proc = subprocess.Popen(
                ["/usr/local/bin/aulos-server", "serve"],
                env=env,
                stdout=log,
                stderr=log,
            )
            try:
                deadline = time.monotonic() + 45
                while True:
                    try:
                        api(base, "/healthz")
                        break
                    except (OSError, ValueError):
                        if proc.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError("server failed to start") from None
                        time.sleep(0.1)
                yield base, Path(temp) / "downloads"
                assert proc.poll() is None, "server exited during smoke"
                api(base, "/healthz")
            except BaseException:
                print(log_path.read_text(), flush=True)
                raise
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)


def check(base, source, path, error=None, download=False):
    url = f"{source}/{path}/{uuid.uuid4().hex}.mp4"
    added = api(base, "/api/v2/downloads", {"url": url, "auto_start": download})
    deadline = time.monotonic() + 90
    expected = "error" if error else "finished" if download else "queued"
    while time.monotonic() < deadline:
        result = api(base, "/api/v2/items/" + added["id"])
        item = result.get("item", result)
        if item["status"] in ("error", "finished", "canceled") or (
            not download and item["status"] == "queued"
        ):
            break
        time.sleep(0.1)
    else:
        raise AssertionError(f"{path} timed out: {item}")
    assert item["status"] == expected, (path, item)
    if error:
        assert error in item["error"]["message"], (path, item)
    else:
        assert item["error"] is None, (path, item)


def run(args):
    assert "Requests" in _REQUEST_HANDLERS, list(_REQUEST_HANDLERS)
    for package in ("yt-dlp", "requests", "urllib3"):
        print(f"{package} {importlib.metadata.version(package)}", flush=True)
    subprocess.run(["python3", "-m", "pip", "check"], check=True, timeout=30)
    with http_server(Source) as port:
        source = f"http://127.0.0.1:{port}"
        for depth in (3, 10):
            with aulos(depth) as (base, downloads):
                for path in (
                    "media/0",
                    "http/1",
                    "http/2",
                    "http/10",
                    "http/11",
                    "http/20",
                    "http/30",
                    "extract/1",
                    "extract/3",
                ):
                    check(base, source, path)
                check(base, source, "http/31", "HTTP Error 302")
                check(base, source, "loop/0", "HTTP Error 302")
                check(base, source, "cycle/0", "Redirect loop at")
                check(
                    base,
                    source,
                    "extract/4",
                    "AULOS_RESOLVE_MAX_DEPTH=3" if depth == 3 else None,
                )
                if depth == 3 and args.stress_batches:
                    cases = [
                        ("http/11", None),
                        ("extract/3", None),
                        ("cycle/0", "Redirect loop at"),
                        ("extract/4", "AULOS_RESOLVE_MAX_DEPTH=3"),
                    ]
                    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                        for _ in range(args.stress_batches):
                            futures = [
                                pool.submit(check, base, source, path, error)
                                for path, error in cases
                            ]
                            for future in futures:
                                future.result(timeout=100)
                if depth == 3 and args.download:
                    with tempfile.TemporaryDirectory() as temp:
                        media = Path(temp) / "sample.mp4"
                        subprocess.run(
                            [
                                "ffmpeg",
                                "-v",
                                "error",
                                "-f",
                                "lavfi",
                                "-i",
                                "color=c=black:s=64x64:r=10",
                                "-t",
                                "1",
                                "-c:v",
                                "libx264",
                                "-pix_fmt",
                                "yuv420p",
                                str(media),
                            ],
                            check=True,
                            timeout=30,
                        )
                        Source.media = media.read_bytes()
                        check(base, source, "http/1", download=True)
                    files = list(downloads.glob("*.mp4"))
                    assert files, "no downloaded media"
                    subprocess.run(
                        ["ffprobe", "-v", "error", str(files[0])],
                        check=True,
                        timeout=15,
                    )
                print(f"redirect API smoke passed at depth {depth}", flush=True)
        Proxy.source_port = port
        with http_server(Proxy) as proxy_port:
            with aulos(3, f"http://127.0.0.1:{proxy_port}") as (base, _):
                for path in ("http/1", "http/11"):
                    before = Proxy.requests
                    check(base, f"http://redirect.test:{port}", path)
                    assert Proxy.requests > before, "proxy was bypassed"
            print("redirect proxy smoke passed", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--stress-batches", type=int, default=0)
    parser.add_argument("--download", action="store_true")
    run(parser.parse_args())
