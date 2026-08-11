#!/data/data/com.termux/files/usr/bin/env bash
# Plan 2.12 (#24): install an agentgrid node daemon on Android/Termux (<5 min).
#
#   pkg install bash curl tar git sqlite   # one-time prerequisites
#   ./install-node-termux.sh --server https://cp.example.com --token <enroll-token> \
#       --adapters fake-acp --binaries ./release-aarch64
#
# Unlike the systemd-targeted install-node.sh, this hits the Android path:
#   - prefix $PREFIX (no /usr/local, no sudo/root).
#   - Hard RSS defaults 256 MiB and max_parallel=1 — Termux runs on battery.
#     Bump with: --max-rss-mib 512 --max-parallel 2.
#   - No user switching: the running Termux user IS the agent.
#   - No systemd. Use `nohup` + a `termux-services`-style script or
#     use `sv-enable agentgrid-noded` if you installed `termux-services`.
set -euo pipefail

SERVER=""
TOKEN=""
NAME="$(getprop ro.product.device 2>/dev/null || echo termux-node)"
BIN_DIR="$PREFIX/bin"
DATA_DIR="$PREFIX/var/lib/agentgrid"
WORKSPACE="$DATA_DIR/workspace"
WORKSPACE_MAX_RSS_MIB=256
WORKSPACE_MAX_PARALLEL=1
ADAPTERS="fake-acp"
STAGING="$(dirname "${BASH_SOURCE[0]}")/release-aarch64"

usage() { echo "usage: $0 --server <url> --token <token> [--name <node>] [--bin-dir <dir>] [--max-rss-mib N] [--max-parallel N] [--adapters fake-acp,...] [--binaries <dir>]"; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --server) SERVER="$2"; shift 2;;
    --token) TOKEN="$2"; shift 2;;
    --name) NAME="$2"; shift 2;;
    --bin-dir) BIN_DIR="$2"; shift 2;;
    --max-rss-mib) WORKSPACE_MAX_RSS_MIB="$2"; shift 2;;
    --max-parallel) WORKSPACE_MAX_PARALLEL="$2"; shift 2;;
    --adapters) ADAPTERS="$2"; shift 2;;
    --binaries) STAGING="$2"; shift 2;;
    *) usage;;
  esac
done
[ -n "$SERVER" ] || usage
[ -n "$TOKEN" ]  || usage

mkdir -p "$BIN_DIR" "$DATA_DIR" "$WORKSPACE"

# Copy binaries from staging. ARCH default assumes aarch64 — Android on
# arm64. The tarball you download from GH releases on Termux is musl-aarch64.
for b in agentgrid-node-daemon agentgrid-agent; do
  if [ -f "$STAGING/$b" ]; then install -m755 "$STAGING/$b" "$BIN_DIR/$b"; else
    echo "warning: $b not found in $STAGING — skipping" >&2
  fi
done
for a in ${ADAPTERS//,/ }; do
  if [ -f "$STAGING/adapter-$a" ]; then install -m755 "$STAGING/adapter-$a" "$BIN_DIR/adapter-$a"; fi
done

# Write the low-power node config.
cat > "$DATA_DIR/config.toml" <<TOML
server = "$SERVER"
workspace_dir = "$WORKSPACE"
# Plan 2.12 (#24): low-power defaults for battery devices. Bump
# max_rss_mib only if you know the device has headroom; Android OOM-kills
# uncapped daemons arbitrary.
max_rss_mib = $WORKSPACE_MAX_RSS_MIB
max_parallel_attempts = $WORKSPACE_MAX_PARALLEL
TOML

echo " wrote $DATA_DIR/config.toml"
echo " start with: nohup agentgrid-agent --config $DATA_DIR/config.toml &"
echo "(or install termux-services and drop a service script under \$PREFIX/var/service/agentgrid-node/)"
