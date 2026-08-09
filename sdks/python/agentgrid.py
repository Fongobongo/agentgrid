"""Agentgrid Python SDK (plan 1.11 / roadmap #8).

Thin client over the /v1 HTTP API - stdlib only (urllib), no dependencies.
Auth: pass a JWT token (from `ag login` or POST /v1/auth/login) or set
AGENTGRID_TOKEN. Usage:

    from agentgrid import Agentgrid

    ag = Agentgrid("http://127.0.0.1:7800", token=os.environ["AGENTGRID_TOKEN"])
    task = ag.run("fix the flaky test", "my-org/my-repo")
    ag.wait(task["id"])
    arts = ag.artifacts(task["id"])
    print(ag.artifact(task["id"], arts[0]["name"]))

Minimal surface: run | wait | cancel | artifacts | artifact | status.
"""

from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request

TERMINAL = {"succeeded", "failed", "cancelled", "blocked"}


class Agentgrid:
    def __init__(self, base: str, token: str | None = None) -> None:
        self.base = base.rstrip("/")
        self.token = token if token is not None else os.environ.get("AGENTGRID_TOKEN", "")

    def _req(self, method: str, path: str, body: object | None = None) -> object:
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        data = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode()
        req = urllib.request.Request(
            f"{self.base}{path}", data=data, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(req) as resp:
                raw = resp.read()
                return json.loads(raw) if raw else None
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"agentgrid {method} {path} -> {e.code} {e.read().decode()}") from e

    def login(self, username: str, password: str) -> str:
        r = self._req("POST", "/v1/auth/login", {"username": username, "password": password})
        self.token = r["token"]  # type: ignore[index]
        return self.token

    def run(
        self,
        prompt: str,
        repository: str,
        *,
        adapter: str = "mock",
        requested_node_id: str | None = None,
        timeout_secs: int | None = None,
        validation_command: str | None = None,
        base_commit: str | None = None,
    ) -> dict:
        return self._req(  # type: ignore[return-value]
            "POST",
            "/v1/tasks",
            {
                "prompt": prompt,
                "repository": repository,
                "adapter": adapter,
                "requested_node_id": requested_node_id,
                "timeout_secs": timeout_secs,
                "validation_command": validation_command,
                "base_commit": base_commit,
            },
        )

    def status(self, id: str) -> str:
        return self._req("GET", f"/v1/tasks/{id}")["status"]  # type: ignore[index]

    def wait(self, id: str, interval_s: float = 2.0, timeout_s: float = 300.0) -> dict:
        deadline = time.monotonic() + timeout_s
        while True:
            t = self._req("GET", f"/v1/tasks/{id}")
            if t["status"] in TERMINAL:  # type: ignore[index]
                return t  # type: ignore[return-value]
            if time.monotonic() > deadline:
                raise TimeoutError(f"agentgrid wait({id}) timed out; last status {t['status']}")
            time.sleep(interval_s)

    def cancel(self, id: str) -> None:
        self._req("POST", f"/v1/tasks/{id}/cancel")

    def artifacts(self, id: str) -> list:
        return self._req("GET", f"/v1/tasks/{id}/artifacts")  # type: ignore[return-value]

    def artifact(self, id: str, name: str) -> str:
        """Download a named artifact as raw text (not JSON)."""
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        req = urllib.request.Request(
            f"{self.base}/v1/tasks/{id}/artifacts/{name}", headers=headers, method="GET"
        )
        try:
            with urllib.request.urlopen(req) as resp:
                return resp.read().decode("utf-8", errors="replace")
        except urllib.error.HTTPError as e:
            raise RuntimeError(f"agentgrid artifact {name} -> {e.code}") from e
