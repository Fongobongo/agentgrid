#!/usr/bin/env bash
# Generate SHA256 checksums for all release binaries.
# Run after `cargo build --release`.

set -euo pipefail

TARGET_DIR="${CARGO_TARGET_DIR:-target}/release"
CHECKSUMS_FILE="$TARGET_DIR/checksums.txt"

BINARIES=(
    "agentgrid-control-plane"
    "agentgrid-node-daemon"
    "ag"
    "agentgrid-gateway"
    "agentgrid-acp-agent"
    "adapter-mock"
    "adapter-claude"
    "adapter-opencode"
    "adapter-fake-acp"
)

echo "Generating checksums in $CHECKSUMS_FILE"
> "$CHECKSUMS_FILE"

for bin in "${BINARIES[@]}"; do
    if [ -f "$TARGET_DIR/$bin" ]; then
        sha256sum "$TARGET_DIR/$bin" >> "$CHECKSUMS_FILE"
        echo "  $bin: $(sha256sum "$TARGET_DIR/$bin" | cut -d' ' -f1)"
    else
        echo "  $bin: not found (skipping)"
    fi
done

echo "Done. Checksums written to $CHECKSUMS_FILE"
cat "$CHECKSUMS_FILE"
