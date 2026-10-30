# Agentgrid SDKs (plan 1.11 / roadmap #8)

Thin clients over the `/v1` HTTP API — embed agentgrid into CI/scripts without
shelling out to `ag run`. Both are dependency-free.

| Language | Location | Runtime |
|---|---|---|
| TypeScript | `sdks/ts/index.ts` | Node >= 18 (built-in `fetch`) |
| Python | `sdks/python/agentgrid.py` | Python >= 3.9 (stdlib only) |

Auth: pass the JWT from `ag login` (or `POST /v1/auth/login`) directly, or set
`AGENTGRID_TOKEN`. The JWT rides as `Authorization: Bearer <token>`.

## Surface

| Method | What it does |
|---|---|
| `run(prompt, repo, opts)` | Create a task; returns the task object |
| `wait(id, interval?, timeout?)` | Poll until terminal (`succeeded`/`failed`/`cancelled`/`blocked`) |
| `status(id)` | Current task status string |
| `cancel(id)` | Cancel a queued/running task |
| `artifacts(id)` | List artifacts of the latest attempt (name + meta) |
| `artifact(id, name)` | Download one artifact's raw content |
| `login(user, pass)` | Login and store the returned JWT |

## `run()` input

`run()` takes **every** field of
[`CreateTaskRequest`](openapi.yaml) — not a hand-picked subset, because a client
that cannot express `max_attempts` or a budget ceiling is not a client of this
API. `prompt` and `repository` are positional; `adapter` is required; the
remaining 23 are optional.

| Field | Type | Meaning |
|---|---|---|
| `adapter` | string | Adapter to run (`opencode`, `claude-code`, `mock`, …) |
| `requested_node_id` | string | Pin the task to one node; unset = scheduler picks |
| `timeout_secs` | int | Per-task timeout; server default when unset |
| `validation_command` | string | Overrides the repository's default validation |
| `base_commit` | string | Exact commit to check out; unset = branch from the default branch |
| `parent_acp_session_id` | string | Resume an ACP session; unset = fresh session |
| `consensus_group_id` / `consensus_member` | string | Consensus fan-out tags (normally stamped by the CP) |
| `consensus_mode` | `"solve"` \| `"review"` | Consensus run mode |
| `review_of` | string | Task id whose `changes.patch` a review group judges |
| `max_tokens` | int | Token ceiling; the attempt is killed with `budget_exceeded` past it |
| `max_cost_cents` | int | Cost ceiling in cents (same semantics) |
| `network_mode` | `"none"` \| `"restricted"` \| `"unrestricted"` | Requested mode; must not exceed the node's maximum |
| `security_profile` | string | Profile name; a `-strict` profile needs structured permission interception |
| `group_id` | string | Shared-context group (`AG_GROUP_ID` on the node) |
| `agent_id` | string | Org-agent attribution; the task counts against that agent's budget |
| `opencode_override` | object | `{ model, small_model, config }`, merged over the node's profile |
| `github_push` | bool | Push the branch, open a PR, comment on the issue (best-effort) |
| `github_repo` | string | `owner/name` for write-back |
| `github_issue` | int | Issue to comment on after a successful write-back |
| `github_base_ref` | string | Base ref the PR targets |
| `github_create` | bool | Create the remote repository if it does not exist (needs `github_push`) |
| `max_attempts` | int | Total attempts allowed; `1` (default) = no auto-retry |
| `retry_on_node_loss` | bool | Whether losing the node may spend the `max_attempts` budget (default `true`) |

Two rules the SDKs enforce:

- **Unset means "server default", so the key is omitted.** The body is built
  from the fields you actually set; an explicit `null` would be a 400 for the
  non-nullable ones (`max_attempts`, `github_push`, `github_create`).
- **`adapter` is required.** It used to default to `mock`, so a caller who
  forgot it got a task that "succeeded" without running an agent.

## Responses

`run()` and `wait()` return the task as the control plane sees it (`TaskView`),
typed as `Task` in both languages. Fields the server omits when unset
(`group_id`, `max_tokens`, `opencode_override`, …) are optional in `Task` —
absent and `null` both mean "unset".

## TypeScript

```ts
import { Agentgrid } from "@agentgrid/sdk";

const ag = new Agentgrid("http://127.0.0.1:7800", process.env.AGENTGRID_TOKEN);
const task = await ag.run("fix the flaky login test", "my-org/my-repo", {
  adapter: "opencode",
  max_attempts: 2,
  max_cost_cents: 200,
  retry_on_node_loss: false,
});
await ag.wait(task.id);
const arts = await ag.artifacts(task.id);
for (const a of arts) {
  console.log(a.name, a.size_bytes, await ag.artifact(task.id, a.name));
}
```

## Python

```python
from agentgrid import Agentgrid

ag = Agentgrid("http://127.0.0.1:7800", token=os.environ["AGENTGRID_TOKEN"])
task = ag.run(
    "fix the flaky login test",
    "my-org/my-repo",
    adapter="opencode",
    max_attempts=2,
    max_cost_cents=200,
    retry_on_node_loss=False,
)
final = ag.wait(task["id"])
assert final["status"] == "succeeded"
for a in ag.artifacts(task["id"]):
    print(a["name"], a["size_bytes"], ag.artifact(task["id"], a["name"]))
```

## E2E

`tests/e2e/run-sdk.sh` starts a local control plane + mock node and drives the
Python SDK end to end (run → wait → artifacts → cancel) with no Docker. Before
any node is online it sends every `CreateTaskRequest` field at once and asserts
the create response, the stored row and the SDK agree — the parity gate for the
table above — then hands that same payload to `tests/e2e/sdk-parity.mts`, which
replays it through the TypeScript SDK against the same control plane (Node ≥
22.6 strips the types, so the SDK stays dependency-free).

The declared field lists are checked by `crates/control-plane/tests/contract.rs`
(`python_sdk_*` / `ts_sdk_*`): adding a field to `CreateTaskRequest` or
`TaskView` without plumbing it through both SDKs fails `cargo test`.

## Notes

- `artifacts(id)` lists the **latest attempt**'s artifacts (`GET
  /v1/tasks/{id}/artifacts`); per-attempt download is
  `GET /v1/tasks/{id}/artifacts/{name}`.
- Tasks with no node online stay `queued` — `wait` keeps polling until the
  timeout.
