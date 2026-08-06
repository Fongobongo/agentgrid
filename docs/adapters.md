# Adapter contract

An **adapter** is the bridge between agentgrid and a real coding agent
(Claude Code, opencode, …). It is a plain executable launched as a subprocess
by the node daemon; everything it reports travels as newline-delimited JSON
(NDJSON) on **stdout**. This document is complete enough to write a new
adapter from scratch.

## Discovery and probing

- Binary name: `adapter-<id>` on `$PATH`, where `<id>` is the adapter id with
  underscores replaced by dashes (`claude_code` → `adapter-claude-code`).
  The exception is `zeroshot`, a built-in cluster adapter backed by the
  container runtime.
- The daemon probes each configured adapter at startup by resolving the
  binary and running `adapter-<id> --version` (stdout, optional — a missing
  or non-standard version just yields `version = null`).
- A missing adapter marks the node **degraded**, not crashed. When the node
  runs a Docker sandbox, the daemon additionally checks the adapter binary
  exists inside the sandbox image.

## Invocation

```
adapter-<id> --prompt "<task prompt>"
```

- `cwd` is the per-attempt git worktree prepared by the daemon. The adapter
  should make all file changes relative to `cwd`.
- API keys and other secrets arrive through the environment, forwarded by the
  daemon from `AGENTGRID_ADAPTER_ENV` (e.g. `ANTHROPIC_API_KEY`). Never read
  them from files outside the worktree.
- `--version` must print a version string and exit 0 (used by the probe).

## Event stream (stdout)

One JSON object per line:

```json
{"type": "<event type>", "payload": { ... }}
```

| `type`        | Stored as | Payload fields                                   | Meaning |
|---------------|-----------|--------------------------------------------------|---------|
| `log`         | `stdout`  | `{"text": string}`                               | Free-form agent output / commentary. |
| `tool_call`   | `tool`    | `{"name": string, "input": object}` or `{"result": value}` | A tool invocation or its result. |
| `file_change` | `artifact`| adapter-defined (e.g. `{"path": string}`)        | A file the agent created/modified. |
| `progress`    | `metric`  | adapter-defined (e.g. token/cost counters)       | Metering / progress signals. |
| `result`      | `result`  | `{"text": string}`                               | Final answer. Emit once, near the end. |
| `error`       | `error`   | `{"text": string}`                               | Fatal failure description. |

Rules:

- `payload` is optional and defaults to `{}`.
- **Unknown `type` values are never fatal**: the daemon stores them as raw
  `stdout` log lines. A future format change degrades, it does not break.
- A non-JSON stdout line is likewise preserved as a raw log line.
- Write line-by-line and flush after each event — the daemon streams them
  live to the control plane.
- The daemon keeps the complete raw stdout as the `agent-raw-output.log`
  artifact regardless of parsing.
- Events flow through the daemon's secret redactor before upload; still, do
  not echo secrets into events.

## stderr

Anything on stderr is forwarded to the daemon log. Use it for diagnostics
(warnings, spawn errors), not for contract events.

## Exit codes

| Exit               | Daemon interpretation |
|--------------------|-----------------------|
| `0`                | attempt succeeded (unless a `result` event carried `is_error`) |
| non-zero           | attempt failed with `error_code = "agent_failed:exit <code>"` |
| killed by signal   | `error_code = "agent_failed:killed by <signal>"` (cancel/timeout path) |
| `127` (convention) | upstream agent binary not found / not spawnable |

## Cancellation and timeouts

- The daemon enforces the task `timeout_secs` itself and cancels by sending
  SIGTERM to the adapter's **process group**, then SIGKILL after a 10 s grace
  period. Adapters need no signal handling, but should spawn children in the
  same process group so the whole tree dies together.
- Do not detach children into new sessions; they will be orphaned on cancel.

## Safety knobs (hardening P0)

- `AGENTGRID_UNSAFE_UNATTENDED=1` — the only switch that may enable
  "skip interactive permissions / auto-run everything" flags
  (e.g. `--dangerously-skip-permissions` for Claude). **Default off.**
  The node daemon refuses to start with this set unless the operator also
  sets `AGENTGRID_I_UNDERSTAND_UNSAFE=1` (explicit acknowledgement).
- Per-adapter knobs (e.g. `AGENTGRID_OPENCODE_AUTO`) may also enable auto-run
  and are loudly warned; prefer the single unsafe knob behind a sandbox.
- In safe mode an unattended run is expected to block on the first
  interactive prompt; that is the safe default.

## Existing adapters

- `adapter-mock` — deterministic fake agent; used by all E2E tests.
- `adapter-claude` — wraps the Claude Code CLI headless
  (`claude -p … --output-format stream-json --verbose`), translates
  `assistant`/`user`/`result` stream-json messages into contract events;
  unknown lines fall through as `log`. Binary overridable via
  `AGENTGRID_CLAUDE_BIN`.
- `adapter-opencode` — same pattern for the opencode CLI.

## Writing a new adapter: checklist

1. Executable named `adapter-<id>` supporting `--version` and
   `--prompt "<text>"`.
2. Emit NDJSON contract events on stdout; flush per line.
3. Gate any permission-bypass flag on `AGENTGRID_UNSAFE_UNATTENDED`
   (helper: `agentgrid_adapters::unsafe_unattended_from_env`); print the
   matching warning via `warn_unsafe`.
4. Keep children in the adapter's process group; exit non-zero on failure.
5. Add unit tests translating recorded samples of the upstream CLI output.
