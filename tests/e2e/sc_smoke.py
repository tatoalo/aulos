import json
import time
import uuid
from http.server import BaseHTTPRequestHandler
from typing import ClassVar
from urllib.parse import urlsplit

from redirect_smoke import api, aulos, http_server


class Site(BaseHTTPRequestHandler):
    tokens: ClassVar[dict[str, float]] = {}
    embeds: ClassVar[list[tuple[str, str | None, float, str]]] = []
    expired_once = False

    def log_message(self, *_):
        pass

    def do_GET(self):
        parsed = urlsplit(self.path)
        parts = parsed.path.strip("/").split("/")
        origin = f"http://127.0.0.1:{self.server.server_port}"
        status = 200
        if parsed.path == "/":
            body = '<div id="app" data-page=\'{"version":"test-version"}\'></div>'
        elif parsed.path.startswith("/it/watch/"):
            case = parts[-1]
            body = json.dumps(
                {
                    "props": {
                        "title": {
                            "id": int(case),
                            "name": "Local test",
                            "type": "movie",
                        },
                        "embedUrl": f"{origin}/iframe/{case}",
                    }
                }
            )
        elif parsed.path.startswith("/iframe/"):
            token = uuid.uuid4().hex
            self.tokens[token] = time.monotonic()
            body = f'<iframe src="{origin}/embed/{parts[-1]}?token={token}"></iframe>'
        elif parsed.path.startswith("/embed/"):
            token = parsed.query.removeprefix("token=")
            referer = self.headers.get("Referer")
            elapsed = time.monotonic() - self.tokens.get(token, 0)
            self.embeds.append((parts[-1], referer, elapsed, token))
            if (
                referer != origin + "/"
                or parts[-1] == "403"
                or (
                    parts[-1] == "503"
                    and sum(case == "503" for case, *_ in self.embeds) > 1
                )
            ):
                status, body = 403, "Forbidden"
            elif elapsed >= 10 or (parts[-1] == "410" and not type(self).expired_once):
                type(self).expired_once = True
                status, body = 410, "Expired"
            else:
                body = f"<script>window.masterPlaylist = {{url: '{origin}/playlist/1', params: {{'token': 'stream-token', 'expires': '9999999999'}}}};</script>"
        else:
            status, body = 404, "Unexpected request"
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def settled(base, item_id, download=False):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        result = api(base, "/api/v2/items/" + item_id)
        item = result.get("item", result)
        if item["status"] in ("error", "finished", "canceled") or (
            not download and item["status"] == "queued"
        ):
            return item
        time.sleep(0.1)
    raise AssertionError("SC extraction timed out")


def run():
    with http_server(Site) as port:
        origin = f"http://127.0.0.1:{port}"
        for client in ("auto", "plain"):
            Site.expired_once = False
            Site.tokens = {}
            Site.embeds = []
            with aulos(
                3,
                extra_env={
                    "AULOS_SC_EXTRA_HOSTS": "127.0.0.1",
                    "AULOS_SC_HTTP": client,
                },
            ) as (base, downloads):
                for case, code, message in (
                    ("200", None, None),
                    ("403", "unavailable", "missing/rejected Referer"),
                    ("410", "network", "embed token expired"),
                    ("503", "unavailable", "missing/rejected Referer"),
                ):
                    added = api(
                        base,
                        "/api/v2/downloads",
                        {
                            "url": f"{origin}/it/watch/{case}",
                            "auto_start": case == "503",
                        },
                    )
                    item = settled(base, added["id"], download=case == "503")
                    provider = item["provider"] or (item["error"] or {}).get("provider")
                    assert provider == "streamingcommunity", item
                    assert item["status"] == ("error" if code else "queued"), item
                    if code:
                        assert item["error"]["code"] == code, item
                        assert message in item["error"]["message"], item
                        assert "token=" not in item["error"]["message"], item
                    else:
                        assert item["error"] is None, item
                logs = (downloads.parent / "server.log").read_text()
                assert "WARN" in logs and "provider job failed" in logs, logs
                assert "streamingcommunity" in logs and "unavailable" in logs, logs
                assert "network" in logs, logs
                assert all(token not in logs for *_, token in Site.embeds), logs
                download_tokens = [
                    token for case, *_, token in Site.embeds if case == "503"
                ]
                assert len(download_tokens) == 2 and len(set(download_tokens)) == 2, (
                    download_tokens
                )
            print(f"StreamingCommunity API smoke passed with {client}", flush=True)
        assert Site.embeds
        assert all(
            referer == origin + "/" and elapsed < 10
            for _, referer, elapsed, _ in Site.embeds
        )


if __name__ == "__main__":
    run()
