#!/usr/bin/env python3
# Run a command on the remote test host over SSH, reading creds from .env.
# Usage:
#   tests/e2e/remote-ssh.py '<shell command>'
#   tests/e2e/remote-ssh.py --file <local_path> '<remote dest path>'
#
# Transport: paramiko (sshpass is unavailable without sudo on the dev box).
# Exit code = remote command exit code (or 2 on connection/setup failure).
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

def main():
    env = {**os.environ, **load_env(ENV)}
    host = env.get("AG_REMOTE_HOST"); user = env.get("AG_REMOTE_USER", "root")
    pw = env.get("AG_REMOTE_PASSWORD"); port = int(env.get("AG_REMOTE_PORT", "22"))
    if not (host and user and pw):
        print("AG_REMOTE_* missing from .env / env", file=sys.stderr); sys.exit(2)
    if len(sys.argv) >= 3 and sys.argv[1] == "--file":
        local, dest = sys.argv[2], sys.argv[3]
        t = paramiko.Transport((host, port)); t.connect(username=user, password=pw)
        sftp = paramiko.SFTPClient.from_transport(t)
        sftp.put(local, dest); sftp.close(); t.close()
        print(f"uploaded {local} -> {user}@{host}:{dest}"); return
    cmd = sys.argv[1] if len(sys.argv) == 2 else " ".join(sys.argv[1:])
    c = paramiko.SSHClient(); c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(host, port=port, username=user, password=pw,
              timeout=15, allow_agent=False, look_for_keys=False)
    i, o, e = c.exec_command(cmd, timeout=600)
    out = o.read().decode(errors="replace")
    err = e.read().decode(errors="replace")
    rc = o.channel.recv_exit_status()
    if out: sys.stdout.write(out)
    if err: sys.stderr.write(err)
    c.close(); sys.exit(rc)

if __name__ == "__main__":
    main()
