#!/usr/bin/env python3
"""Local Windows smoke for the telegram-gateway e2e flow (dev aid only).

The bash e2e (run-telegram-gateway.sh) runs on CI's Linux runners. This
script reproduces the same scenario on a Windows dev box in one process:
CP + mock Bot API + gateway as subprocesses, then assertions. Run from the
repo root: python tests/e2e/local_telegram_smoke.py
"""
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CP_PORT = 7802
TG_PORT = 19081
CHAT_ID = 4242
USER = "admin"
PASS = "changeme"

def http(method, url, body=None, token=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=10) as r:
        raw = r.read()
    return r.status, (json.loads(raw) if raw else None)

def wait_ready(url, tries=60):
    for _ in range(tries):
        try:
            http("GET", url)
            return True
        except Exception:
            time.sleep(0.5)
    return False

def main():
    tmp = tempfile.mkdtemp(prefix="ag-tg-smoke-")
    art = os.path.join(tmp, "art")
    os.makedirs(art, exist_ok=True)
    hits = os.path.join(tmp, "hits.jsonl")

    # 1) Control plane
    env = dict(os.environ)
    env.update(
        AGENTGRID_LISTEN=f"127.0.0.1:{CP_PORT}",
        AGENTGRID_DB=os.path.join(tmp, "cp.db"),
        AGENTGRID_JWT_SECRET="smoke-secret",
        AGENTGRID_ARTIFACT_ROOT=art,
    )
    cp_log = open(os.path.join(tmp, "cp.out"), "wb")
    cp = subprocess.Popen(
        [os.path.join(ROOT, "target", "debug", "agentgrid-control-plane.exe")],
        env=env, stdout=cp_log, stderr=subprocess.STDOUT)
    base = f"http://127.0.0.1:{CP_PORT}"
    assert wait_ready(base + "/health/ready"), "CP not ready"

    log = open(os.path.join(tmp, "cp.out"), "rb").read().decode(errors="replace")
    m = re.search(r"=== agentgrid setup token.*===\s*\n\s*([A-Za-z0-9_-]+)", log)
    assert m, "setup token not found in CP log"
    http("POST", base + "/v1/auth/setup",
         {"username": USER, "password": PASS, "setup_token": m.group(1)})
    _, login = http("POST", base + "/v1/auth/login",
                    {"username": USER, "password": PASS})
    jwt = login["token"]

    # 2) Mock Bot API
    mock_log = open(os.path.join(tmp, "mock.out"), "wb")
    mock = subprocess.Popen(
        [sys.executable, os.path.join(ROOT, "tests", "e2e", "mini-telegram-bot.py"),
         str(TG_PORT), hits],
        stdout=mock_log, stderr=subprocess.STDOUT)
    for _ in range(40):
        try:
            s = socket.create_connection(("127.0.0.1", TG_PORT), 0.3)
            s.close()
            break
        except OSError:
            time.sleep(0.2)
    else:
        raise AssertionError("mock Bot API did not come up")

    # 3) A task + a pending approval
    _, task = http("POST", base + "/v1/tasks",
                   {"prompt": "tg smoke", "repository": "demo", "adapter": "mock"},
                   token=jwt)
    task_id = task["id"]
    _, appr = http("POST", base + f"/v1/tasks/{task_id}/approvals",
                   {"attempt_id": "smoke-attempt", "permission": "bash",
                    "scope": "repo:demo"}, token=jwt)
    approval_id = appr["id"]
    print(f"  task {task_id[:8]} approval {approval_id[:8]}")

    # 4) Gateway
    genv = dict(os.environ)
    genv.update(
        AGENTGRID_GATEWAY_ADMINS=str(CHAT_ID),
        AGENTGRID_TELEGRAM_API=f"http://127.0.0.1:{TG_PORT}",
    )
    gw_log = open(os.path.join(tmp, "gw.out"), "wb")
    gw = subprocess.Popen(
        [os.path.join(ROOT, "target", "debug", "agentgrid-gateway.exe"),
         "run", "--control-plane", base, "--token", jwt, "--telegram", "smoke-token"],
        env=genv, stdout=gw_log, stderr=subprocess.STDOUT)
    time.sleep(1.5)

    def enqueue(kind, payload_text):
        if kind == "cmd":
            upd = {"update_id": int(time.time() * 1000) % 10**9,
                   "message": {"chat": {"id": CHAT_ID}, "text": payload_text}}
        else:
            upd = {"update_id": int(time.time() * 1000) % 10**9,
                   "callback_query": {"id": "cbq1", "data": payload_text,
                                       "message": {"chat": {"id": CHAT_ID}, "text": "tap"},
                                       "from": {"id": CHAT_ID}}}
        http("POST", f"http://127.0.0.1:{TG_PORT}/enqueue", upd)

    def hits_records():
        try:
            with open(hits, encoding="utf-8") as f:
                return [json.loads(l) for l in f]
        except FileNotFoundError:
            return []

    # 5) /nodes smoke
    enqueue("cmd", "/nodes")
    deadline = time.time() + 10
    while time.time() < deadline:
        if any("online" in (h.get("text") or "") or "no nodes" in (h.get("text") or "")
               for h in hits_records() if h.get("kind") == "send"):
            print("  /nodes ok")
            break
        time.sleep(0.3)
    else:
        raise AssertionError("/nodes reply never arrived")

    # 6) /approvals carries the permission + inline keyboard
    enqueue("cmd", "/approvals")
    deadline = time.time() + 10
    kb = None
    while time.time() < deadline:
        recs = hits_records()
        for h in recs:
            if h.get("kind") == "send" and "bash" in (h.get("text") or ""):
                kb = h.get("reply_markup")
        if kb:
            break
        time.sleep(0.3)
    assert kb, "approvals reply missing the keyboard"
    flat = json.dumps(kb)
    assert f"ag:allow:{approval_id}" in flat and f"ag:deny:{approval_id}" in flat, \
        "approval id missing on buttons"
    print("  /approvals + keyboard ok")

    # 7) deny tap -> CP-side deny + ack
    enqueue("cb", f"ag:deny:{approval_id}")
    deadline = time.time() + 10
    while time.time() < deadline:
        _, a = http("GET", base + f"/v1/approvals/{approval_id}", token=jwt)
        if a["status"] == "denied":
            break
        time.sleep(0.3)
    else:
        raise AssertionError("approval not denied after tap")
    assert any(h.get("kind") == "ack" for h in hits_records()), "callback not acked"
    print("  deny tap ok")

    print("SMOKE OK")
    for p in (gw, mock, cp):
        p.kill()

if __name__ == "__main__":
    main()
