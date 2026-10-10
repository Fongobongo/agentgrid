#!/usr/bin/env bash
# Conformant stub for the `zeroshot` executor CLI (Stage 10 E2E contract, see
# `crates/node-daemon/src/zeroshot.rs`). Lets CI exercise the node ↔ cluster
# path (run → stream → export/kill) with zero external dependencies; a real
# binary must implement the same subcommands. Honest simplifications vs a real
# executor are marked STUB below.
#
# Subcommands:
#   zeroshot --version
#   zeroshot run --id <attempt> --workdir <dir> [--role executor|verifier]
#     --prompt <prompt>
#   zeroshot kill <attempt>          (idempotent; STUB: marker file only — the
#     node already SIGTERMs the `run` process group, so there is no live
#     cluster left to reap)
#   zeroshot export <attempt> --dir <dir>
#
# Prompt directives (one per line, mock-adapter style):
#   sleep:<sec>   block (cancel/timeout path; traps TERM and exits 143)
#   fail:<code>   emit an error event and exit with <code>
# Any other line is echoed as a note. A final `result` event is always
# emitted on the success path.
#
# Observability markers in ${ZEROSHOT_STUB_STATE:-/tmp/zeroshot-stub-state}:
#   started-<id>, killed-<id>, exported-<id> (+ role-<id> when --role passed).
set -euo pipefail

STATE="${ZEROSHOT_STUB_STATE:-/tmp/zeroshot-stub-state}"
mkdir -p "$STATE"

emit() { # $1 = type, $2 = compact JSON payload
  printf '{"type":"%s","payload":%s}\n' "$1" "$2"
}

if [ "${1:-}" = "--version" ] || [ "${1:-}" = "-v" ]; then
  echo "zeroshot 0.1.0-stub"
  exit 0
fi

case "${1:-}" in
  run)
    shift
    ID=""; WORKDIR=""; ROLE=""; PROMPT=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --id) ID="$2"; shift 2;;
        --workdir) WORKDIR="$2"; shift 2;;
        --role) ROLE="$2"; shift 2;;
        --prompt) PROMPT="$2"; shift 2;;
        *) echo "stub: unknown run flag $1" >&2; exit 2;;
      esac
    done
    [ -n "$ID" ] || { echo "stub: run needs --id" >&2; exit 2; }
    [ -d "$WORKDIR" ] || { echo "stub: workdir missing: $WORKDIR" >&2; exit 2; }
    touch "$STATE/started-$ID"
    [ -n "$ROLE" ] && echo "$ROLE" >"$STATE/role-$ID"
    # TERM trap: a node cancel SIGTERMs the process group; exit promptly so
    # the node observes the kill instead of hanging to the timeout.
    trap 'exit 143' TERM
    cd "$WORKDIR"
    emit log "{\"text\":\"zeroshot-stub cluster $ID starting\"}"
    [ -n "$ROLE" ] && emit log "{\"text\":\"role side: $ROLE\"}"
    CODE=0
    while IFS= read -r line || [ -n "$line" ]; do
      text="$(printf '%s' "$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
      [ -z "$text" ] && continue
      case "$text" in
        sleep:*)
          secs="${text#sleep:}"
          emit log "{\"text\":\"sleeping ${secs}s\"}"
          sleep "$secs" || CODE=143
          ;;
        fail:*)
          CODE="${text#fail:}"
          emit error "{\"text\":\"stub forced failure exit_code=$CODE\"}"
          ;;
        *)
          emit log "{\"text\":\"note: $text\"}"
          ;;
      esac
      [ "$CODE" != "0" ] && break
    done <<<"$PROMPT"
    if [ "$CODE" = "0" ]; then
      echo "zeroshot-stub output for $ID" >zeroshot-output.txt
      emit file_change '{"path":"zeroshot-output.txt","bytes":32}'
      last="$(printf '%s' "$PROMPT" | grep -v '^[[:space:]]*$' | tail -1)"
      esc="$(printf '%s' "$last" | sed 's/"/\\"/g')"
      emit result "{\"text\":\"stub result: $esc\"}"
    fi
    exit "$CODE"
    ;;
  kill)
    # STUB: no live cluster exists beyond the `run` process the node already
    # signalled; record the call so E2E can assert cancel-total ran.
    touch "$STATE/killed-${2:?usage: zeroshot kill <id>}"
    exit 0
    ;;
  export)
    shift
    ID=""; DIR=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --dir) DIR="$2"; shift 2;;
        *) if [ -z "$ID" ]; then ID="$1"; else echo "stub: bad export arg $1" >&2; exit 2; fi; shift;;
      esac
    done
    [ -n "$ID" ] && [ -d "$DIR" ] || { echo "stub: export needs <id> --dir <dir>" >&2; exit 2; }
    role="null"
    [ -f "$STATE/role-$ID" ] && role="\"$(cat "$STATE/role-$ID")\""
    printf '{"cluster_id":"%s","executor":"zeroshot-stub","role_side":%s,"status":"exported"}\n' "$ID" "$role" >"$DIR/zeroshot-result.json"
    touch "$STATE/exported-$ID"
    exit 0
    ;;
  *)
    echo "stub: unknown subcommand ${1:-}" >&2
    exit 2
    ;;
esac
