#!/usr/bin/env python3
"""A dependency-free WebSocket reader for the e2e harness (PROTOCOL §5).

`tests/e2e/run.sh` has to assert the `added -> delta -> completed` sequence for one item id, and
the two obvious ways to do that both fail in a CI container: `websocat` is not installed and the
image ships no Python WebSocket library. RFC 6455 client framing is about sixty lines, so this is
the third way -- no dependencies, no image change, and it runs on the host as well as inside the
container.

What it does, and deliberately no more:

* the HTTP/1.1 upgrade handshake with ``Sec-WebSocket-Protocol: aulos.v2``;
* reads text frames (fragmented ones reassembled), replies to ``ping`` so a connection can outlive
  the server's 20 s keepalive, and ignores everything else;
* prints ``READY`` once the socket is up, so the driver can start the socket **before** the add
  and therefore see a real ``added`` frame rather than a ``snapshot``;
* prints one line per frame as ``<t> <seq>`` so the shell can grep the order;
* exits 0 as soon as the frames named by ``--expect`` have all been seen **for the item**, in
  order; exits 1 on timeout, printing what it did see.

The item is named either by id (``--item``) or, when the driver does not know the id yet because
the add has not happened, by a substring of its URL (``--match``): the first ``added`` frame whose
item URL contains it latches that item's id, and every later frame is matched on the id — which is
all a ``delta`` carries (PROTOCOL §5.4).

Usage:
    ws_watch.py --url ws://127.0.0.1:8081/ws --match "watch?v=aqz" \
                --expect added,delta,completed [--timeout 300] [--token BEARER]
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import socket
import struct
import sys
import time
from urllib.parse import urlparse

TEXT, BINARY, CLOSE, PING, PONG, CONT = 0x1, 0x2, 0x8, 0x9, 0xA, 0x0


def handshake(sock: socket.socket, host: str, path: str, token: str | None) -> bytes:
    """Performs the RFC 6455 client handshake and returns any bytes read past the headers.

    Raises on anything but a `101`.
    """
    key = base64.b64encode(os.urandom(16)).decode()
    protocol = "aulos.v2" + (f", bearer.{token}" if token else "")
    request = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        f"Sec-WebSocket-Protocol: {protocol}\r\n"
        "\r\n"
    )
    sock.sendall(request.encode())
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("the server closed the connection during the handshake")
        buffer += chunk
    head, _, rest = buffer.partition(b"\r\n\r\n")
    status = head.split(b"\r\n", 1)[0].decode(errors="replace")
    if "101" not in status:
        raise RuntimeError(f"upgrade refused: {status}\n{head.decode(errors='replace')}")
    # Anything already read past the headers is the first frame's bytes.
    sock.setblocking(True)
    return rest


def send_frame(sock: socket.socket, opcode: int, payload: bytes = b"") -> None:
    """Sends one masked client frame (a client MUST mask, RFC 6455 §5.3)."""
    header = bytearray([0x80 | opcode])
    mask = os.urandom(4)
    length = len(payload)
    if length < 126:
        header.append(0x80 | length)
    elif length < (1 << 16):
        header.append(0x80 | 126)
        header += struct.pack(">H", length)
    else:
        header.append(0x80 | 127)
        header += struct.pack(">Q", length)
    header += mask
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(bytes(header) + masked)


class Reader:
    """Buffers bytes off the socket and yields (opcode, payload) frames."""

    def __init__(self, sock: socket.socket, prefetched: bytes) -> None:
        self.sock = sock
        self.buf = bytearray(prefetched)

    def _need(self, n: int) -> None:
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("the server closed the connection")
            self.buf += chunk

    def frame(self) -> tuple[int, bytes]:
        self._need(2)
        first, second = self.buf[0], self.buf[1]
        opcode = first & 0x0F
        masked = bool(second & 0x80)
        length = second & 0x7F
        offset = 2
        if length == 126:
            self._need(offset + 2)
            length = struct.unpack(">H", bytes(self.buf[offset : offset + 2]))[0]
            offset += 2
        elif length == 127:
            self._need(offset + 8)
            length = struct.unpack(">Q", bytes(self.buf[offset : offset + 8]))[0]
            offset += 8
        if masked:  # a server never masks, but be explicit rather than silently wrong
            self._need(offset + 4)
            offset += 4
        self._need(offset + length)
        payload = bytes(self.buf[offset : offset + length])
        del self.buf[: offset + length]
        return opcode, payload


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--item", default=None, help="the item id the frames must mention")
    parser.add_argument(
        "--match",
        dest="match_url",
        default=None,
        help="a URL substring; the first `added` frame carrying it latches the item id",
    )
    parser.add_argument("--expect", default="added,delta,completed")
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument("--token", default=os.environ.get("AULOS_API_TOKEN") or None)
    args = parser.parse_args()
    if not args.item and not args.match_url:
        parser.error("one of --item or --match is required")

    parsed = urlparse(args.url)
    host = parsed.hostname or "127.0.0.1"
    port = parsed.port or (443 if parsed.scheme == "wss" else 80)
    path = parsed.path or "/"
    if parsed.query:
        path += "?" + parsed.query

    expect = [name.strip() for name in args.expect.split(",") if name.strip()]
    remaining = list(expect)
    seen: list[str] = []

    sock = socket.create_connection((host, port), timeout=10)
    prefetched = handshake(sock, f"{host}:{port}", path, args.token)
    reader = Reader(sock, prefetched)
    target = args.item
    # The driver waits for this before adding, so no `added` frame can be missed.
    print("READY", flush=True)

    deadline = time.monotonic() + args.timeout
    pending = ""
    while remaining:
        left = deadline - time.monotonic()
        if left <= 0:
            print(
                f"TIMEOUT after {args.timeout}s; still waiting for {remaining}; saw {seen}",
                file=sys.stderr,
            )
            return 1
        sock.settimeout(min(left, 30.0))
        try:
            opcode, payload = reader.frame()
        except socket.timeout:
            continue
        except RuntimeError as e:
            print(f"CLOSED: {e}; still waiting for {remaining}; saw {seen}", file=sys.stderr)
            return 1

        if opcode == PING:
            send_frame(sock, PONG, payload)
            continue
        if opcode in (PONG,):
            continue
        if opcode == CLOSE:
            print(f"CLOSE frame; still waiting for {remaining}; saw {seen}", file=sys.stderr)
            return 1
        if opcode == CONT:
            pending += payload.decode("utf-8", "replace")
        elif opcode == TEXT:
            pending = payload.decode("utf-8", "replace")
        else:
            continue

        try:
            frame = json.loads(pending)
        except json.JSONDecodeError:
            # A fragmented text frame: keep buffering.
            continue
        pending = ""

        kind = frame.get("t", "?")
        seq = frame.get("seq")
        text = json.dumps(frame)

        # Latch the id off the first `added`/`snapshot` frame that carries the URL we were told to
        # look for. `delta` frames carry only the id, so the latch has to happen here.
        if target is None and args.match_url and args.match_url in text:
            for row in frame.get("items", []) or []:
                if args.match_url in json.dumps(row):
                    target = row.get("id")
                    print(f"item {target}", flush=True)
                    break

        mentions = target is not None and target in text
        print(f"{kind} {seq}{' *' if mentions else ''}", flush=True)
        seen.append(kind)
        if remaining and kind == remaining[0] and mentions:
            remaining.pop(0)

    send_frame(sock, CLOSE, struct.pack(">H", 1000))
    sock.close()
    print(f"OK: saw {expect} in order for {target}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:  # noqa: BLE001 - the harness wants one line, not a traceback
        print(f"ws_watch failed: {e}", file=sys.stderr)
        sys.exit(1)
