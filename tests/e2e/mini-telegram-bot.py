#!/usr/bin/env python3
"""Tiny mock Telegram Bot API for the gateway e2e (no external deps).

Listens on HTTP. Implemented methods:
- POST /bot<token>/getUpdates   — long-poll; each queued update is returned
  once, then acked by offset. An empty queue waits up to `timeout` seconds
  (short-circuited to ~1s so the e2e stays fast) and returns [].
- POST /bot<token>/sendMessage — records {chat_id, text, reply_markup?} to
  the hits file (one JSON per line); always 200 {"ok":true}.
- POST /bot<token>/answerCallbackQuery — recorded like sendMessage with a
  `kind: "ack"` marker; always 200.
- POST /bot<token>/editMessageText — accepted silently (future use).

The hits file doubles as the test oracle: the script asserts on allow/deny
hits + sendMessage texts.

Usage: mini-telegram-bot.py PORT HITS_FILE
"""
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
HITS = sys.argv[2]

_lock = threading.Lock()
_updates = []  # [{"update_id": n, ...}]
_offset = [0]

def _record(kind, body):
    with _lock:
        with open(HITS, "a", encoding="utf-8") as f:
            f.write(json.dumps({"kind": kind, **body}) + "\n")

def enqueue(update):
    """Queue an update for the next getUpdates call (e2e control hook)."""
    with _lock:
        _updates.append(update)

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silence
        pass

    def _json(self, code, obj):
        payload = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b"{}"
        try:
            return json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return {}

    def do_POST(self):
        parts = self.path.strip("/").split("/")
        # Control hook: queue an update for the next getUpdates call.
        if len(parts) == 1 and parts[0] == "enqueue":
            enqueue(self._body())
            return self._json(200, {"ok": True})
        if len(parts) != 3 or parts[0] != "bot":
            return self._json(404, {"ok": False, "error": "not found"})
        method = parts[2]
        body = self._body()

        if method == "getUpdates":
            timeout = min(float(body.get("timeout", 0) or 0), 1.0)
            deadline = time.time() + timeout
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
            return self._json(200, {"ok": True, "result": pending})

        if method == "sendMessage":
            _record("send", body)
            return self._json(200, {"ok": True, "result": {"message_id": 1}})

        if method == "answerCallbackQuery":
            _record("ack", body)
            return self._json(200, {"ok": True, "result": True})

        return self._json(404, {"ok": False, "error": f"unknown method {method}"})

if __name__ == "__main__":
    open(HITS, "w").close()  # truncate so each run starts clean
    srv = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    srv.serve_forever()
