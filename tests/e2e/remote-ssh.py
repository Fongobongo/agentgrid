#!/usr/bin/env python3
# Run a command on the remote test host over SSH, reading creds from .env.
# Usage:
#   tests/e2e/remote-ssh.py '<shell command>'
#   tests/e2e/remote-ssh.py --file <local_path> '<remote dest path>'
#
# Transport: paramiko (sshpass is unavailable without sudo on the dev box).
# Auth: SSH key first (AG_REMOTE_KEY, default ~/.ssh/id_ed25519_agentgrid_remote);
# the password path remains only as a bootstrap to install a new key and is
# refused when no key exists AND no password is configured. Exit code =
# remote command exit code (or 2 on connection/setup failure).
import os, sys, pathlib, paramiko

ROOT = pathlib.Path(__file__).resolve().parents[2]
ENV = ROOT / ".env"

def load_env(path):
    out = {}
    if not path.exists():
        return out
    for ln in path.read_text().splitlines():
        ln = ln.strip()
        if not ln or ln.startswith("#") or "=" not in ln:
            continue
        k, v = ln.split("=", 1)
        out[k.strip()] = v.strip()
    return out

def connect(host, port, user, env):
    key_path = env.get("AG_REMOTE_KEY") or str(pathlib.Path.home() / ".ssh/id_ed25519_agentgrid_remote")
    pw = env.get("AG_REMOTE_PASSWORD")
    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    # Key auth preferred; fall back to password only when explicitly configured.
    look_for_keys = pathlib.Path(key_path).exists()
    c.connect(
        host, port=port, username=user,
        key_filename=key_path if look_for_keys else None,
        password=pw if not look_for_keys else None,
        timeout=15, allow_agent=False,
        look_for_keys=False,
    )
    return c

def main():
    env = {**os.environ, **load_env(ENV)}
    host = env.get("AG_REMOTE_HOST"); user = env.get("AG_REMOTE_USER", "root")
    port = int(env.get("AG_REMOTE_PORT", "22"))
    key_path = env.get("AG_REMOTE_KEY") or str(pathlib.Path.home() / ".ssh/id_ed25519_agentgrid_remote")
    if not host or not (pathlib.Path(key_path).exists() or env.get("AG_REMOTE_PASSWORD")):
        print("no auth: install ~/.ssh/id_ed25519_agentgrid_remote (or set AG_REMOTE_PASSWORD for bootstrap)", file=sys.stderr)
        sys.exit(2)
    if len(sys.argv) >= 3 and sys.argv[1] == "--file":
        local, dest = sys.argv[2], sys.argv[3]
        t = paramiko.Transport((host, port))
        pk = paramiko.Ed25519Key.from_private_key_file(key_path) if pathlib.Path(key_path).exists() else None
        t.connect(username=user, pkey=pk, password=None if pk else env.get("AG_REMOTE_PASSWORD"))
        sftp = paramiko.SFTPClient.from_transport(t)
        sftp.put(local, dest); sftp.close(); t.close()
        print(f"uploaded {local} -> {user}@{host}:{dest}"); return
    cmd = sys.argv[1] if len(sys.argv) == 2 else " ".join(sys.argv[1:])
    c = connect(host, port, user, env)
    i, o, e = c.exec_command(cmd, timeout=600)
    out = o.read().decode(errors="replace")
    err = e.read().decode(errors="replace")
    rc = o.channel.recv_exit_status()
    if out: sys.stdout.write(out)
    if err: sys.stderr.write(err)
    c.close(); sys.exit(rc)

if __name__ == "__main__":
    main()
