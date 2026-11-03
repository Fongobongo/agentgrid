#!/usr/bin/env bash
# Join this host to an AgentGrid control plane.
#
#   curl -fsSL https://cp.example.com/v1/node/join.sh | sudo bash -s -- --token <t>
#
# The control plane serves this file at /v1/node/join.sh with its own URL
# substituted for the placeholder below, so the line above is the whole
# procedure. It fetches the daemon (and the adapters you name) from the control
# plane, verifies every byte against the bundle's SHA-256, and hands the
# staging directory to the same installer the release tarball ships — so the
# pull path and the tarball path run one implementation, not two that drift.
#
# This is the opposite direction from `ag nodes install`, which pushes over ssh
# and therefore needs credentials *to* the host. Here the host only has to
# reach the control plane, which it must do anyway to work.
#
# The token is minted by the panel (Nodes -> Install) or by
# `POST /v1/nodes/enrollment-token`. It is single-use and expires in 10
# minutes, which is what makes a copy-pasted command safe to hand out.
set -euo pipefail

# Single quotes: the value is substituted per request from the request's Host
# header, and this file is piped into `sudo bash`. The control plane validates
# the host before substituting it; the quoting here is the second line of
# defence, not the only one.
SERVER='__AGENTGRID_SERVER__'
TOKEN=""
NAME="$(hostname 2>/dev/null || echo node)"
ADAPTERS="mock"
DATA_DIR=""
BIN_DIR=""
DRY_RUN=0
KEEP=0
PROBE=0

usage() {
  cat >&2 <<EOF
usage: join.sh --token <enrollment-token> [--server <url>] [--name <node>]
               [--adapters mock[,claude,...]] [--data-dir <dir>] [--bin-dir <dir>]
               [--dry-run] [--probe] [--keep]

  --token      enrollment token (single-use, expires in 10 minutes)
  --server     control plane URL (default: ${SERVER})
  --adapters   adapters to install from the bundle (default: mock)
  --data-dir   node data directory (default: the installer's /var/lib/agentgrid)
  --bin-dir    where the binaries go (default: /usr/local/bin)
  --dry-run    fetch and verify everything, install nothing
  --probe      check what this host can actually reach, before installing
               anything. Through a relay the CONTROL PLANE line reports
               hops=1, which is the proof that the relay forwards
  --keep       keep the staging directory (for debugging a failed install)
EOF
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --token)    TOKEN="${2:-}";    shift 2 2>/dev/null || usage;;
    --server)   SERVER="${2:-}";   shift 2 2>/dev/null || usage;;
    --name)     NAME="${2:-}";     shift 2 2>/dev/null || usage;;
    --adapters) ADAPTERS="${2:-}"; shift 2 2>/dev/null || usage;;
    --data-dir) DATA_DIR="${2:-}"; shift 2 2>/dev/null || usage;;
    --bin-dir)  BIN_DIR="${2:-}";  shift 2 2>/dev/null || usage;;
    --dry-run)  DRY_RUN=1; shift 1;;
    --probe)    PROBE=1; shift 1;;
    --keep)     KEEP=1; shift 1;;
    -h|--help)  usage;;
    *) echo "join.sh: unknown argument '$1'" >&2; usage;;
  esac
done

[ -n "$TOKEN" ] || { echo "join.sh: --token is required" >&2; usage; }
case "$SERVER" in
  *__AGENTGRID_SERVER__*)
    echo "join.sh: no control plane URL. Pass --server <url>, or fetch this script" >&2
    echo "         from the control plane: curl -fsSL <cp>/v1/node/join.sh | sudo bash -s -- --token <t>" >&2
    exit 2;;
esac
SERVER="${SERVER%/}"
command -v curl >/dev/null 2>&1 || { echo "join.sh: curl is required" >&2; exit 1; }
command -v sha256sum >/dev/null 2>&1 || { echo "join.sh: sha256sum is required" >&2; exit 1; }
if [ "$DRY_RUN" = "0" ] && [ "$(id -u)" != "0" ]; then
  echo "join.sh: run as root — curl -fsSL $SERVER/v1/node/join.sh | sudo bash -s -- --token <t>" >&2
  exit 1
fi

echo ">> reading the node bundle from $SERVER"
MANIFEST="$(curl -fsSL "$SERVER/v1/node/bundle")" || {
  echo "join.sh: cannot read $SERVER/v1/node/bundle" >&2
  echo "         Is the control plane reachable from this host, and is AGENTGRID_NODE_DIST set?" >&2
  exit 1
}

# The bundle is built on the control plane's platform. Refusing an arch
# mismatch here beats installing a daemon that dies with "Exec format error".
WANT_ARCH="$(printf '%s\n' "$MANIFEST" | awk -F'[ =]' '/^# arch=/{print $3; exit}')"
HAVE_ARCH="$(uname -m)"
case "$HAVE_ARCH" in arm64) HAVE_ARCH=aarch64;; esac
if [ -n "$WANT_ARCH" ] && [ "$WANT_ARCH" != "$HAVE_ARCH" ]; then
  echo "join.sh: this control plane serves $WANT_ARCH binaries; this host is $HAVE_ARCH." >&2
  echo "         Install from the release tarball for $HAVE_ARCH instead, or point" >&2
  echo "         AGENTGRID_NODE_DIST at a build for this architecture." >&2
  exit 1
fi

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/agentgrid-join.XXXXXX")"
cleanup() {
  if [ "$KEEP" = "1" ]; then
    echo ">> staging kept at $STAGE"
  else
    # A failed cleanup must not turn a successful join into a failed one.
    rm -rf "$STAGE" 2>/dev/null || true
  fi
}
trap cleanup EXIT

: >"$STAGE/checksums.txt"
COUNT=0
while read -r NAME_ONLY SIZE SHA; do
  case "$NAME_ONLY" in ''|'#'*) continue;; esac
  case "$NAME_ONLY" in
    agentgrid-node-daemon|agentgrid-node-daemon.exe) ;;
    adapter-*)
      # Only the adapters that were asked for: the bundle may hold more, and
      # installing a wrapper adapter for a CLI this host does not have produces
      # a node that advertises work it cannot run.
      case ",$ADAPTERS," in
        *",${NAME_ONLY#adapter-},"*) ;;
        *) continue;;
      esac;;
    *) continue;;
  esac
  echo ">> $NAME_ONLY ($SIZE bytes)"
  curl -fsSL "$SERVER/v1/node/bundle/$NAME_ONLY" -o "$STAGE/$NAME_ONLY" || {
    echo "join.sh: download of $NAME_ONLY failed" >&2; exit 1; }
  # `sha256sum -c` wants "<digest>  <file>"; the installer runs it in $STAGE.
  printf '%s  %s\n' "$SHA" "$NAME_ONLY" >>"$STAGE/checksums.txt"
  COUNT=$((COUNT + 1))
done <<<"$MANIFEST"

[ "$COUNT" -gt 0 ] || {
  echo "join.sh: the bundle served nothing this host can install" >&2; exit 1; }
for a in ${ADAPTERS//,/ }; do
  grep -q "^adapter-$a " <<<"$MANIFEST" || \
    echo "join.sh: adapter '$a' is not in the bundle; skipping it" >&2
done

# Verify here rather than leaving it to the installer: `--dry-run` never reaches
# the installer, and a dry run that reports "verified" without verifying would
# be worse than no dry run at all. The installer repeats the check (it is the
# one that installs), which costs one hash of a few tens of MB.
if ! ( cd "$STAGE" && sha256sum -c --quiet checksums.txt ); then
  echo "join.sh: the downloaded files do not match the bundle's SHA-256." >&2
  echo "         Nothing was installed. Re-run; if it repeats, the control plane's" >&2
  echo "         AGENTGRID_NODE_DIST changed while this host was downloading." >&2
  exit 1
fi

# Ask the host itself what it can reach, using the binary we just verified and
# before anything is installed. `--server` is the URL this host was told to dial:
# if that is a relay, the probe's CONTROL PLANE line reports hops=1, which is the
# only way to tell a relay that forwards from a proxy that merely listens. The
# probe needs no token and no credential, so this adds no trust in the bundle
# beyond the SHA-256 already checked.
if [ "$PROBE" = "1" ]; then
  echo ">> probing from this host: what can it reach?"
  chmod +x "$STAGE/agentgrid-node-daemon"
  if ! "$STAGE/agentgrid-node-daemon" probe --server "$SERVER"; then
    echo "join.sh: this host cannot reach $SERVER. Nothing was installed." >&2
    echo "         If the control plane is unreachable from here, the join has to" >&2
    echo "         go through a relay: on the control plane, run" >&2
    echo "           ag nodes add --name $NAME --via <node>" >&2
    echo "         and paste the line it prints instead of this one." >&2
    exit 1
  fi
fi

echo ">> fetching the installer"
curl -fsSL "$SERVER/v1/node/install.sh" -o "$STAGE/install-node.sh" || {
  echo "join.sh: cannot read $SERVER/v1/node/install.sh" >&2; exit 1; }
chmod +x "$STAGE/install-node.sh"

INSTALL_ARGS=(--server "$SERVER" --token "$TOKEN" --name "$NAME"
              --staging "$STAGE" --adapters "$ADAPTERS")
if [ -n "$DATA_DIR" ]; then INSTALL_ARGS+=(--data-dir "$DATA_DIR"); fi
if [ -n "$BIN_DIR" ]; then INSTALL_ARGS+=(--bin-dir "$BIN_DIR"); fi

if [ "$DRY_RUN" = "1" ]; then
  echo ">> dry run: $COUNT file(s) verified against the bundle's SHA-256"
  echo "   would run: bash $STAGE/install-node.sh ${INSTALL_ARGS[*]}"
  exit 0
fi

bash "$STAGE/install-node.sh" "${INSTALL_ARGS[@]}"
