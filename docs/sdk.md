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
| `run(prompt, repo, opts?)` | Create a task; returns the task object |
| `wait(id, interval?, timeout?)` | Poll until terminal (`succeeded`/`failed`/`cancelled`/`blocked`) |
| `status(id)` | Current task status string |
| `cancel(id)` | Cancel a queued/running task |
| `artifacts(id)` | List artifacts of the latest attempt (name + meta) |
| `artifact(id, name)` | Download one artifact's raw content |
| `login(user, pass)` | Login and store the returned JWT |

## TypeScript

```ts
import { Agentgrid } from "@agentgrid/sdk";

const ag = new Agentgrid("http://127.0.0.1:7800", process.env.AGENTGRID_TOKEN);
const task = await ag.run("fix the flaky login test", "my-org/my-repo", {
  adapter: "mock",
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
task = ag.run("fix the flaky login test", "my-org/my-repo", adapter="mock")
final = ag.wait(task["id"])
assert final["status"] == "succeeded"
for a in ag.artifacts(task["id"]):
    print(a["name"], a["size_bytes"], ag.artifact(task["id"], a["name"]))
```

## E2E

`tests/e2e/run-sdk.sh` starts a local control plane + mock node and drives the
Python SDK end to end (run → wait → artifacts → cancel) with no Docker.

## Notes

- `artifacts(id)` lists the **latest attempt**'s artifacts (`GET
  /v1/tasks/{id}/artifacts`); per-attempt download is
  `GET /v1/tasks/{id}/artifacts/{name}`.
- Tasks with no node online stay `queued` — `wait` keeps polling until the
  timeout.
