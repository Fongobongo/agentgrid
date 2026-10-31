#!/usr/bin/env python3
"""Minimal HTTP forward proxy for e2e (absolute-form requests + CONNECT).

Handles plain-HTTP forward-proxy requests (`GET http://host:port/path`) and
`CONNECT` tunnels, and appends one line per request to a hit-log.

`CONNECT` is what the WebSocket transport needs: the node opens a tunnel and
performs the upgrade inside it, so a proxy that only forwards absolute-form
requests can carry polling and nothing else. The hit-log distinguishes the two
("CONNECT host:port" vs "GET http://…"), which is how the ws-proxy E2E proves
the tunnel was used instead of a direct dial.

Usage: mini-forward-proxy.py <listen-port> <hit-log>
"""
import http.server
import select
import socket
import sys
import urllib.error
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

    def do_CONNECT(self):
        host, _, port = self.path.rpartition(":")
        try:
            upstream = socket.create_connection((host, int(port)), timeout=10)
        except (OSError, ValueError):
            self.send_error(502, "cannot reach the CONNECT target")
            return
        with open(HITLOG, "a") as f:
            f.write(f"CONNECT {self.path}\n")
        # Written straight to the socket: `send_response` would add
        # Server/Date headers, and anything after the blank line belongs to
        # the tunnelled protocol.
        self.wfile.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        self.wfile.flush()
        self.connection.settimeout(None)
        upstream.settimeout(None)
        try:
            while True:
                ready, _, _ = select.select([self.connection, upstream], [], [], 30)
                if not ready:
                    # An idle tunnel is a healthy tunnel: the WS channel pings
                    # every ~15 s but a poll gap can be longer.
                    continue
                for sock in ready:
                    data = sock.recv(65536)
                    if not data:
                        return
                    (upstream if sock is self.connection else self.connection).sendall(
                        data
                    )
        except OSError:
            return
        finally:
            upstream.close()
            self.close_connection = True

    def log_message(self, *a):
        pass


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


Server(("127.0.0.1", PORT), Handler).serve_forever()
