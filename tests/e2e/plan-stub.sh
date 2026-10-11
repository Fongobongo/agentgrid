#!/usr/bin/env bash
# Deterministic stand-in for a planning-capable agent (one-button assembly
# E2E, no LLM). Speaks the wrapper adapter contract: reads `--prompt` (or
# AGENTGRID_PROMPT), emits one `plan` event carrying a fixed worker plan,
# then a `result` event, and exits 0. The node captures the plan via the
# existing `note_plan` path, so the run pauses `PlanReady` (or auto-expands)
# exactly like with a real architect.
#
# The fixed plan builds two mock workers (mock adapter must be installed
# alongside on the node); the system appends the collector automatically.
# Override the emitted plan with PLAN_STUB_YAML when a variant is needed.
set -euo pipefail

if [ "${1:-}" = "--version" ] || [ "${1:-}" = "-v" ]; then
  echo "plan-stub 0.1.0"
  exit 0
fi

PROMPT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prompt) PROMPT="$2"; shift 2;;
    *) shift;;
  esac
done
[ -n "$PROMPT" ] || PROMPT="${AGENTGRID_PROMPT:-}"

PLAN="${PLAN_STUB_YAML:-$(cat <<'EOF'
- id: w1
  prompt: "write:part-a.txt:AAA"
  role: worker
  adapter: mock
- id: w2
  prompt: "write:part-b.txt:BBB"
  role: worker
  adapter: mock
EOF
)}"

PLAN="$PLAN" python3 -c 'import json,os;print(json.dumps({"type":"plan","payload":{"text":os.environ["PLAN"]}}))'
python3 -c 'import json;print(json.dumps({"type":"result","payload":{"text":"stub architect planned 2 workers"}}))'
exit 0
