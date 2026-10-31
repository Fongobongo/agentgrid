#!/usr/bin/env python3
"""Minimal ACP client that drives `agentgrid-acp-agent` over stdio.

`agentgrid-acp-agent` is the northbound ACP gateway: it lets an external ACP
*client* create Agentgrid tasks (`session/prompt` mints a task and streams its
events back as `session/update` until the task is terminal). It ships in every
release tarball and, before this script existed, nothing ran it against a real
control plane — the only coverage was four in-process unit tests against a fake
CP.

Speaks just enough JSON-RPC 2.0 for the lifecycle: `initialize` ->
`session/new` -> `session/prompt`. Prints one JSON summary on stdout and exits
non-zero if the task did not succeed or no update was streamed.

Usage:
  acp-gateway-client.py --bin BIN --server URL [--token T] [--agent mock]
                        [--cwd DIR] [--prompt TEXT] [--timeout SECS]
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import threading


class Client:
    def __init__(self, proc):
        self.proc = proc
        self.msgs = queue.Queue()
        self.updates = []
        self.permissions = 0
        self._next_id = 0
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._drain_stderr, daemon=True).start()

    def _read_stdout(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.msgs.put(json.loads(line))
            except json.JSONDecodeError:
                self.msgs.put({"__unparsed__": line})
        self.msgs.put({"__eof__": True})

    def _drain_stderr(self):
        # The server logs to stderr; not draining it would block the child once
        # the pipe buffer fills.
        for _ in self.proc.stderr:
            pass

    def _write(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params, timeout):
        self._next_id += 1
        rid = self._next_id
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            try:
                msg = self.msgs.get(timeout=timeout)
            except queue.Empty:
                raise SystemExit(f"timeout after {timeout}s waiting for {method}")
            if "__eof__" in msg:
                raise SystemExit(f"agent closed the stream while waiting for {method}")
            if "__unparsed__" in msg:
                continue
            # Server-initiated request: answer it, or `session/prompt` blocks
            # forever waiting for our reply.
            if "method" in msg and "id" in msg:
                self.permissions += 1
                self._write(
                    {"jsonrpc": "2.0", "id": msg["id"], "result": {"allowed": False}}
                )
                continue
            if "method" in msg:
                if msg["method"] == "session/update":
                    self.updates.append(msg.get("params", {}).get("update"))
                continue
            if msg.get("id") == rid:
                if "error" in msg:
                    raise SystemExit(f"{method} failed: {msg['error']}")
                return msg.get("result", {})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--server", required=True)
    ap.add_argument("--token", default=None)
    ap.add_argument("--agent", default="mock")
    ap.add_argument("--cwd", default=os.getcwd())
    ap.add_argument("--prompt", default="acp-gateway e2e")
    ap.add_argument("--timeout", type=int, default=120)
    args = ap.parse_args()

    env = dict(os.environ)
    env["AGENTGRID_SERVER"] = args.server
    if args.token:
        env["AGENTGRID_TOKEN"] = args.token

    proc = subprocess.Popen(
        [args.bin],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=env,
    )
    try:
        c = Client(proc)

        init = c.request(
            "initialize",
            {
                "protocol_version": "1",
                "agent": args.agent,
                "model": "mock",
                "cwd": args.cwd,
            },
            args.timeout,
        )
        if not init.get("protocol_version"):
            raise SystemExit(f"initialize returned no protocol_version: {init}")

        session = c.request(
            "session/new", {"agent": args.agent, "cwd": args.cwd}, args.timeout
        )
        session_id = session.get("session_id")
        if not session_id:
            raise SystemExit(f"session/new returned no session_id: {session}")

        result = c.request(
            "session/prompt",
            {"session_id": session_id, "prompt": args.prompt},
            args.timeout,
        )

        summary = {
            "protocol_version": init.get("protocol_version"),
            "session_id": session_id,
            "status": result.get("status"),
            "error_code": result.get("error_code"),
            "updates": len(c.updates),
            "permissions": c.permissions,
        }
        print(json.dumps(summary))
        return 0 if summary["status"] == "succeeded" and summary["updates"] > 0 else 1
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
