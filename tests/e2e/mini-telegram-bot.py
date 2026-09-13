#!/usr/bin/env python3
"""Tiny mock Telegram Bot API for the gateway e2e (raw sockets, no deps).

The stdlib http.server turned out to be a poor dance partner for reqwest's
connection pooling (bodies intermittently failed to decode, keep-alive
races produced 1000 req/s spin loops). This mock speaks just enough HTTP
on raw sockets with full control over bytes and framing.

Methods:
- POST /bot<token>/getUpdates   — long-poll (capped at 1s); each queued
  update is handed out once, then acked via offset.
- POST /bot<token>/sendMessage  — recorded to the hits file (kind=send).
- POST /bot<token>/answerCallbackQuery — recorded (kind=ack).
- POST /enqueue                 — control hook: queue an update.

Every response uses `Connection: close` and the connection IS closed —
reqwest re-dials per request, which it handles reliably.

Usage: mini-telegram-bot.py PORT HITS_FILE
"""
import json
import socket
import sys
import threading
import time

PORT = int(sys.argv[1])
HITS = sys.argv[2]

_lock = threading.Lock()
_updates = []
_offset = [0]

def _record(kind, body):
    with _lock:
        with open(HITS, "a", encoding="utf-8") as f:
            f.write(json.dumps({"kind": kind, **body}) + "\n")

def _respond(sock, obj):
    payload = json.dumps(obj).encode()
    head = (
        "HTTP/1.1 200 OK\r\n"
        "Content-Type: application/json\r\n"
        f"Content-Length: {len(payload)}\r\n"
        "Connection: close\r\n"
        "\r\n"
    ).encode()
    sock.sendall(head + payload)

def _respond_404(sock):
    body = b'{"ok": false}'
    head = (
        "HTTP/1.1 404 Not Found\r\n"
        f"Content-Length: {len(body)}\r\n"
        "Connection: close\r\n"
        "\r\n"
    ).encode()
    sock.sendall(head + body)

def _read_request(sock):
    """Read until the header block ends, then exactly Content-Length bytes."""
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(65536)
        if not chunk:
            return None, None
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    cl = 0
    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            cl = int(line.split(b":", 1)[1].strip())
    body = rest
    while len(body) < cl:
        chunk = sock.recv(65536)
        if not chunk:
            break
        body += chunk
    path = head.split(b"\r\n")[0].split(b" ")[1].decode()
    try:
        parsed = json.loads(body[:cl] if cl else b"{}")
    except json.JSONDecodeError:
        parsed = {}
    return path, parsed

def _handle(sock):
    try:
        path, body = _read_request(sock)
        if path is None:
            return
        if path == "/enqueue":
            with _lock:
                _updates.append(body)
            return _respond(sock, {"ok": True})
        # Bot API method path: /bot<token>/<method>. The token itself may
        # contain anything (including '/'-free junk in tests) — parse by
        # prefix, not by splitting on '/'.
        if not path.startswith("/bot"):
            return _respond_404(sock)
        rest = path[len("/bot"):]
        method = rest.split("/", 1)[1] if "/" in rest else ""
        if method == "getUpdates":
            timeout = min(float(body.get("timeout", 0) or 0), 1.0)
            deadline = time.time() + timeout
            pending = []
            while time.time() < deadline:
                with _lock:
                    pending = [u for u in _updates if u["update_id"] >= _offset[0]]
                if pending:
                    break
                time.sleep(0.05)
            with _lock:
                pending = [u for u in _updates if u["update_id"] >= _offset[0]]
                for u in pending:
                    _offset[0] = max(_offset[0], u["update_id"] + 1)
            return _respond(sock, {"ok": True, "result": pending})
        if method == "sendMessage":
            _record("send", body)
            return _respond(sock, {"ok": True, "result": {"message_id": 1}})
        if method == "answerCallbackQuery":
            _record("ack", body)
            return _respond(sock, {"ok": True, "result": True})
        _respond_404(sock)
    finally:
        try:
            sock.close()
        except OSError:
            pass

def main():
    open(HITS, "w").close()  # truncate so each run starts clean
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", PORT))
    srv.listen(64)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=_handle, args=(conn,), daemon=True).start()

if __name__ == "__main__":
    main()
