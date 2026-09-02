#!/usr/bin/env python3
"""Minimal HTTP forward proxy for e2e (absolute-form requests only).

Handles plain-HTTP forward-proxy requests (`GET http://host:port/path`),
forwards them, and appends one line per request to a hit-log. This is all
the node poll loop needs (local CP is plain HTTP; no CONNECT/TLS).

Usage: mini-forward-proxy.py <listen-port> <hit-log>
"""
import http.server
import sys
import urllib.request

PORT = int(sys.argv[1])
HITLOG = sys.argv[2]


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _forward(self, body=None):
        req = urllib.request.Request(self.path, data=body, method=self.command)
        for k, v in self.headers.items():
            if k.lower() not in ("host", "proxy-connection", "content-length"):
                req.add_header(k, v)
        with open(HITLOG, "a") as f:
            f.write(f"{self.command} {self.path}\n")
        try:
            with urllib.request.urlopen(req, timeout=100) as r:
                out = r.read()
                self.send_response(r.status)
                self.send_header("Content-Length", str(len(out)))
                self.end_headers()
                self.wfile.write(out)
        except urllib.error.HTTPError as e:
            out = e.read()
            self.send_response(e.code)
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0) or 0)
        self._forward(self.rfile.read(n) if n else None)

    def do_GET(self):
        self._forward()

    def log_message(self, *a):
        pass


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


Server(("127.0.0.1", PORT), Handler).serve_forever()
