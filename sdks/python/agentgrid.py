"""Agentgrid Python SDK (plan 1.11 / roadmap #8).

Thin client over the /v1 HTTP API - stdlib only (urllib), no dependencies.
Auth: pass a JWT token (from `ag login` or POST /v1/auth/login) or set
AGENTGRID_TOKEN. Usage:

    from agentgrid import Agentgrid

    ag = Agentgrid("http://127.0.0.1:7800", token=os.environ["AGENTGRID_TOKEN"])
    task = ag.run("fix the flaky test", "my-org/my-repo", adapter="opencode")
    ag.wait(task["id"])
    arts = ag.artifacts(task["id"])
    print(ag.artifact(task["id"], arts[0]["name"]))

Surface: run | wait | cancel | artifacts | artifact | status.

`run()` exposes every field of `CreateTaskRequest` (docs/openapi.yaml), not a
hand-picked subset: a thin client that silently drops `max_attempts` or
`retry_on_node_loss` cannot express what the API can. A field left at `None`
is *omitted* from the body rather than sent as JSON null, so the server's own
default applies — sending an explicit null for a non-Option field
(`max_attempts`, `github_push`, `github_create`) is a 400, not a default.
"""

from __future__ import annotations

import json
import os
import time
import urllib.error
import urllib.request
from typing import Any, Literal, TypedDict

TERMINAL = {"succeeded", "failed", "cancelled", "blocked"}


class OpencodeOverride(TypedDict, total=False):
    """Per-task opencode config override (`OpencodeOverride` in the API).

    Merged shallowly over the node's active profile and injected as
    OPENCODE_CONFIG_CONTENT, so it dies with the adapter process.
    """

    model: str
    small_model: str
    config: dict[str, Any]


class Task(TypedDict, total=False):
    """A task as the control plane returns it (`TaskView`).

    Every key is optional: the server omits fields that are null for this
    task (`skip_serializing_if`), so `task.get("group_id")` is the honest
    read — an absent key and an explicit null both mean "unset".
    """

    id: str
    repository: str
    prompt: str
    adapter: str
    status: str
    created_at: str
    finished_at: str | None
    assigned_attempt_id: str | None
    validation_command: str | None
    error_code: str | None
    requested_node_id: str | None
    base_commit: str | None
    parent_acp_session_id: str | None
    network_mode: str | None
    security_profile: str | None
    group_id: str | None
    agent_id: str | None
    consensus_group_id: str | None
    consensus_member: str | None
    consensus_mode: str | None
    review_of: str | None
    opencode_override: OpencodeOverride
    github_repo: str | None
    github_issue: int | None
    github_base_ref: str | None
    github_create: bool
    rework_of: str | None
    max_attempts: int
    max_tokens: int | None
    max_cost_cents: int | None


class ArtifactMeta(TypedDict, total=False):
    """One artifact of the latest attempt (`ArtifactMeta` in the API)."""

    name: str
    size_bytes: int
    media_type: str | None
    sha256: str | None


def _compact(body: dict[str, Any]) -> dict[str, Any]:
    """Drop unset fields so the server default applies (see the module doc)."""
    return {k: v for k, v in body.items() if v is not None}


class Agentgrid:
    def __init__(self, base: str | list[str], token: str | None = None) -> None:
        self.bases = [b.rstrip("/") for b in (base if isinstance(base, list) else [base])]
        if not self.bases:
            raise ValueError("agentgrid: at least one control-plane URL is required")
        self.token = token if token is not None else os.environ.get("AGENTGRID_TOKEN", "")

    @property
    def base(self) -> str:
        return self.bases[0]

    def _prefer(self, hint: str | None) -> None:
        if not hint:
            return
        url = hint.rstrip("/")
        if url in self.bases[1:]:
            self.bases.remove(url)
            self.bases.insert(0, url)

    def _req(self, method: str, path: str, body: object | None = None) -> object:
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        data = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode()
        last_error: Exception | None = None
        # Snapshot the order: following a leader hint reorders `bases` for
        # later calls, but this pass must still try each replica once.
        bases = list(self.bases)
        for i, base in enumerate(bases):
            req = urllib.request.Request(
                f"{base}{path}", data=data, headers=headers, method=method
            )
            try:
                with urllib.request.urlopen(req) as resp:
                    raw = resp.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as e:
                if e.code == 503 and i + 1 < len(bases):
                    self._prefer(e.headers.get("X-AgentGrid-Leader"))
                    last_error = e
                    continue
                raise RuntimeError(
                    f"agentgrid {method} {path} -> {e.code} {e.read().decode()}"
                ) from e
            except urllib.error.URLError as e:
                last_error = e
                continue
        raise RuntimeError(f"agentgrid {method} {path}: no reachable control plane") from last_error

    def login(self, username: str, password: str) -> str:
        r = self._req("POST", "/v1/auth/login", {"username": username, "password": password})
        self.token = r["token"]  # type: ignore[index]
        return self.token

    def run(
        self,
        prompt: str,
        repository: str,
        *,
        adapter: str,
        requested_node_id: str | None = None,
        timeout_secs: int | None = None,
        validation_command: str | None = None,
        base_commit: str | None = None,
        parent_acp_session_id: str | None = None,
        consensus_group_id: str | None = None,
        consensus_member: str | None = None,
        consensus_mode: Literal["solve", "review"] | None = None,
        review_of: str | None = None,
        max_tokens: int | None = None,
        max_cost_cents: int | None = None,
        network_mode: Literal["none", "restricted", "unrestricted"] | None = None,
        security_profile: str | None = None,
        group_id: str | None = None,
        agent_id: str | None = None,
        opencode_override: OpencodeOverride | None = None,
        github_push: bool | None = None,
        github_repo: str | None = None,
        github_issue: int | None = None,
        github_base_ref: str | None = None,
        github_create: bool | None = None,
        max_attempts: int | None = None,
        retry_on_node_loss: bool | None = None,
    ) -> Task:
        """Create a task (`POST /v1/tasks`) and return it as the CP sees it.

        `prompt`, `repository` and `adapter` are the only fields the API
        requires. Everything else is an optional refinement that falls back to
        the server's own default when omitted: `max_attempts` 1 (no retry),
        `retry_on_node_loss` true, `github_push` / `github_create` false,
        unbounded `max_tokens` / `max_cost_cents`.

        `consensus_group_id` / `consensus_member` / `consensus_mode` /
        `review_of` are normally stamped by the control plane when one prompt
        is fanned out to N adapters (`ag run --consensus`); they are accepted
        here because the API accepts them, not because a client usually sets
        them.
        """
        body = {
            "prompt": prompt,
            "repository": repository,
            "adapter": adapter,
            "requested_node_id": requested_node_id,
            "timeout_secs": timeout_secs,
            "validation_command": validation_command,
            "base_commit": base_commit,
            "parent_acp_session_id": parent_acp_session_id,
            "consensus_group_id": consensus_group_id,
            "consensus_member": consensus_member,
            "consensus_mode": consensus_mode,
            "review_of": review_of,
            "max_tokens": max_tokens,
            "max_cost_cents": max_cost_cents,
            "network_mode": network_mode,
            "security_profile": security_profile,
            "group_id": group_id,
            "agent_id": agent_id,
            "opencode_override": opencode_override,
            "github_push": github_push,
            "github_repo": github_repo,
            "github_issue": github_issue,
            "github_base_ref": github_base_ref,
            "github_create": github_create,
            "max_attempts": max_attempts,
            "retry_on_node_loss": retry_on_node_loss,
        }
        return self._req("POST", "/v1/tasks", _compact(body))  # type: ignore[return-value]

    def status(self, id: str) -> str:
        return self._req("GET", f"/v1/tasks/{id}")["status"]  # type: ignore[index]

    def wait(self, id: str, interval_s: float = 2.0, timeout_s: float = 300.0) -> Task:
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

    def artifacts(self, id: str) -> list[ArtifactMeta]:
        return self._req("GET", f"/v1/tasks/{id}/artifacts")  # type: ignore[return-value]

    def artifact(self, id: str, name: str) -> str:
        """Download a named artifact as raw text (not JSON)."""
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        last_error: Exception | None = None
        bases = list(self.bases)
        for i, base in enumerate(bases):
            req = urllib.request.Request(
                f"{base}/v1/tasks/{id}/artifacts/{name}", headers=headers, method="GET"
            )
            try:
                with urllib.request.urlopen(req) as resp:
                    return resp.read().decode("utf-8", errors="replace")
            except urllib.error.HTTPError as e:
                if e.code == 503 and i + 1 < len(bases):
                    self._prefer(e.headers.get("X-AgentGrid-Leader"))
                    last_error = e
                    continue
                raise RuntimeError(f"agentgrid artifact {name} -> {e.code}") from e
            except urllib.error.URLError as e:
                last_error = e
                continue
        raise RuntimeError(f"agentgrid artifact {name}: no reachable control plane") from last_error
