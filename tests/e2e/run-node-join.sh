#!/usr/bin/env bash
# Pull-based node join — the `curl | sudo bash` path — against a real control
# plane.
#
#   bash tests/e2e/run-node-join.sh
#
# The join runs in `--dry-run --probe`: it fetches the bundle manifest,
# downloads every file it would install, verifies each against the manifest's
# SHA-256, runs the verified daemon against the control plane to report what the
# host can actually reach, and prints the installer command it would run.
# Installing for real needs root, systemd and an `agentgrid` user; that half is
# `deploy/install-node.sh`, which `run-clean-host.sh` exercises on a clean host.
# What this script proves is the part that is new and could silently be wrong:
# the four endpoints, the allowlist, that the bytes a host downloads are the
# bytes the control plane hashed, and that `--probe` really executes them.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
PORT="${AGENTGRID_PORT:-7831}"
BASE="http://127.0.0.1:$PORT"
USER=admin
PASS=dev-only-password

TMP="$(mktemp -d -t ag-e2e-join-XXXXXX)"
mkdir -p "$TMP/dist"

# A dist dir we control: the two files a join needs, plus three it must never
# hand out. AGENTGRID_NODE_DIST is operator-chosen, so "whatever is in this
# folder" would be a reader for any file the control plane can open.
cp "$BIN/agentgrid-node-daemon" "$TMP/dist/"
cp "$BIN/adapter-mock" "$TMP/dist/"
echo "not for the joiner" >"$TMP/dist/secret.txt"
echo "TOKEN=leak" >"$TMP/dist/.env"
echo "# notes" >"$TMP/dist/notes.md"
export AGENTGRID_NODE_DIST="$TMP/dist"

source "$ROOT/tests/e2e/lib-bootstrap.sh"
source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; tail -30 "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

fail() { echo "FAIL: $*"; exit 1; }

echo ">> the manifest describes what a host can install"
manifest="$(curl -fsS "$BASE/v1/node/bundle")"
head -1 <<<"$manifest" | grep -q '^# arch=' || fail "manifest has no arch line: $(head -1 <<<"$manifest")"
grep -q '^agentgrid-node-daemon ' <<<"$manifest" || fail "daemon missing from the manifest"
grep -q '^adapter-mock ' <<<"$manifest" || fail "adapter-mock missing from the manifest"
for leak in secret.txt .env notes.md; do
  grep -q "^$leak " <<<"$manifest" && fail "$leak leaked into the manifest"
done
echo "   $(grep -vc '^#' <<<"$manifest") file(s) offered, nothing else"

echo ">> the allowlist holds over HTTP, not just in the listing"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/node/bundle/agentgrid-node-daemon")
[ "$code" = "200" ] || fail "daemon download returned $code"
for leak in secret.txt notes.md .env; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/node/bundle/$leak")
  [ "$code" = "404" ] || fail "$leak returned $code, expected 404"
done
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/node/bundle/..%2F..%2Fetc%2Fpasswd")
[ "$code" = "404" ] || fail "a traversal path returned $code, expected 404"
echo "   daemon 200, everything else 404"

echo ">> the served script already knows this control plane"
script="$(curl -fsS "$BASE/v1/node/join.sh")"
grep -q "__AGENTGRID_SERVER__" <<<"$script" && fail "the placeholder was served unsubstituted"
# Single quotes, not double: the value is substituted per request from the
# request's Host header, and the script is piped into `sudo bash`.
grep -q "SERVER='$BASE'" <<<"$script" || fail "the script does not default to $BASE"
grep -q "# WARNING: this control plane answered over plain http" <<<"$script" \
    || fail "an http fetch must warn inside the script it serves"
curl -fsS "$BASE/v1/node/install.sh" | grep -q -- '--staging' || fail "install.sh is not the installer"
echo "   one line, no URL to type"

echo ">> dry run: fetch, verify, probe, stop"
out="$(bash "$ROOT/deploy/join-node.sh" --server "$BASE" --token "$ENROLL_TOKEN" --dry-run --probe --adapters mock 2>&1)" || {
  echo "$out"; fail "the join script failed"; }
grep -q "dry run: 2 file(s) verified" <<<"$out" || { echo "$out"; fail "expected 2 verified files"; }
grep -q -- '--staging' <<<"$out" || { echo "$out"; fail "no installer command printed"; }
grep -q -- "--token $ENROLL_TOKEN" <<<"$out" || { echo "$out"; fail "the token was not passed through"; }
# `--probe` runs the daemon the script just verified, before anything is
# installed. A direct join reports hops=0; through a relay the same line would
# read hops=1, which is the whole point of the flag.
grep -q "probing from this host" <<<"$out" || { echo "$out"; fail "--probe did not run"; }
grep -q "CONTROL PLANE" <<<"$out" || { echo "$out"; fail "the probe printed no control-plane line"; }
grep -q "hops=0" <<<"$out" || { echo "$out"; fail "a direct join must report no forwarder"; }
echo "   daemon + adapter-mock downloaded and verified"
echo "   the staged daemon reached the control plane before installing anything"

echo ">> a digest that does not match the bytes is refused"
# The control plane hashes a file when it builds the manifest and reads it again
# when it serves it, so a live CP cannot be made to disagree with itself. A stub
# that serves the real bytes under a wrong digest can: that is the case the
# script has to catch, and it is the whole reason `curl | bash` is acceptable
# here.
STUB_PORT=$((PORT + 1))
STUB_ARCH="$(uname -m)"
case "$STUB_ARCH" in arm64) STUB_ARCH=aarch64;; esac
python3 - "$TMP/dist" "$STUB_PORT" "$STUB_ARCH" <<'PYSTUB' &
import hashlib, http.server, os, socketserver, sys

root, port, arch = sys.argv[1], int(sys.argv[2]), sys.argv[3]

def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/v1/node/bundle":
            names = ["agentgrid-node-daemon", "adapter-mock"]
            lines = [f"# arch={arch} os=linux"]
            for name in names:
                path = os.path.join(root, name)
                # Honest for the daemon, deliberately wrong for the adapter.
                sha = digest(path) if name != "adapter-mock" else "0" * 64
                lines.append(f"{name} {os.path.getsize(path)} {sha}")
            body = ("\n".join(lines) + "\n").encode()
            self.send_response(200)
            self.send_header("content-type", "text/plain")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.startswith("/v1/node/bundle/"):
            path = os.path.join(root, self.path.rsplit("/", 1)[-1])
            if not os.path.isfile(path):
                self.send_error(404)
                return
            data = open(path, "rb").read()
            self.send_response(200)
            self.send_header("content-type", "application/octet-stream")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        self.send_error(404)

socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", port), Handler) as httpd:
    httpd.serve_forever()
PYSTUB
STUB_PID=$!
E2E_PIDS="$E2E_PIDS $STUB_PID"
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$STUB_PORT/v1/node/bundle" >/dev/null 2>&1 && break
  sleep 0.25
done

out2="$(bash "$ROOT/deploy/join-node.sh" --server "http://127.0.0.1:$STUB_PORT" \
  --token "$ENROLL_TOKEN" --dry-run --adapters mock 2>&1 || true)"
grep -q "dry run:" <<<"$out2" && { echo "$out2"; fail "a wrong digest was accepted"; }
grep -q "do not match" <<<"$out2" || { echo "$out2"; fail "no checksum failure reported"; }
echo "   wrong digest caught before anything is installed"

echo ">> a join without a token is refused before it touches anything"
if bash "$ROOT/deploy/join-node.sh" --server "$BASE" --dry-run >/dev/null 2>&1; then
  fail "the join script ran without --token"
fi
echo "   usage error, exit 2"

echo "NODE JOIN E2E OK"
