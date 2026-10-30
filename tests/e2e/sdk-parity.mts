// Shared-body parity driver for the TypeScript SDK, run by `run-sdk.sh`
// against a live control plane (no node needed: every task is cancelled).
//
// The Python block in `run-sdk.sh` builds one payload containing every field of
// `CreateTaskRequest`, checks the echo, and writes that payload to a file; this
// driver replays the *same* body through the TS SDK and asserts the same echo.
// One payload, two clients — otherwise the two blocks drift, and the TS SDK had
// no runtime coverage at all (its `cancel()` threw on the CP's `200` + empty
// body for years' worth of releases).
//
// Run with Node >= 22.6: `node --experimental-strip-types sdk-parity.mts <base> <payload.json>`

import { readFileSync } from "node:fs";

import { Agentgrid } from "../../sdks/ts/index.ts";

const [base, payloadPath] = process.argv.slice(2);
if (!base || !payloadPath) {
  throw new Error("usage: sdk-parity.mts <base-url> <payload.json>");
}

const sent = JSON.parse(readFileSync(payloadPath, "utf8")) as Record<string, unknown>;
// TaskView does not echo these three: they are scheduling inputs, not state.
const NOT_ECHOED = new Set(["timeout_secs", "github_push", "retry_on_node_loss"]);

const ag = new Agentgrid(base, process.env.AGENTGRID_TOKEN ?? "");
const task = await ag.run("parity-ts", "*", sent as never);
const created = task as unknown as Record<string, unknown>;

let checked = 0;
for (const [k, v] of Object.entries(sent)) {
  if (NOT_ECHOED.has(k)) continue;
  const got = created[k];
  if (JSON.stringify(got) !== JSON.stringify(v)) {
    throw new Error(`TS echo ${k}: sent ${JSON.stringify(v)}, got ${JSON.stringify(got)}`);
  }
  checked++;
}

// Unset fields must be omitted, not nulled (a null is a 400 for `max_attempts`
// / `github_push` / `github_create`).
const minimal = await ag.run("minimal-ts", "*", { adapter: "mock" });
if (minimal.max_attempts !== 1) throw new Error(`max_attempts default: ${minimal.max_attempts}`);
if (minimal.github_create !== false) throw new Error("github_create default");
if ("group_id" in minimal) throw new Error("group_id must be absent when unset");

// `cancel()` is the call that used to throw on the CP's empty 200.
await ag.cancel(minimal.id);
await ag.cancel(task.id);

console.log(`TS SDK parity OK: ${checked} fields echoed; unset omitted; cancel ok`);
