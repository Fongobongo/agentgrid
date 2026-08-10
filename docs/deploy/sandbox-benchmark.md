# Sandbox cold-start benchmark (plan 2.3, roadmap #9)

Records the `submit→running` latency a task incurs with `AGENTGRID_SANDBOX`
= `none` vs `docker`/`podman`, and how to read the delta as the sandbox
cold-start cost. Run it before deciding whether to pre-warm containers.

## How to run

1. Start a control plane and one node daemon (with the sandbox image pre-pulled
   `docker pull $AGENTGRID_SANDBOX_IMAGE`, default
   `ghcr.io/agentgrid/agent-sandbox:latest`).
2. Run the benchmark — it drives the control plane twice, once per sandbox
   mode, and prints `submit_ms` per iteration plus a per-mode average:

```bash
# First with the node under AGENTGRID_SANDBOX=none (legacy).
AGENTGRID_CONTROL=http://127.0.0.1:7800 \
AG_AGENT_API_TOKEN=<token> \
./deploy/sandbox-benchmark.sh 5
# Restart the node daemon with AGENTGRID_SANDBOX=docker and press Enter to
# record the sandbox= docker section.
```

3. The full-wall time per mode is the `submit_ms` average. Isolating the
   pure container-start overhead needs the node's spawn span — grep for a
   tracing line on the node (`sandbox_spawn_start/end`) or add a temporary
   `std::time::Instant` around `sandbox_command` in
   `crates/node-daemon/src/attempt_runner.rs::spawn_via_backend` — the
   script-driven delta is the honest number the operator experiences.

## Interpreting results

- **End-to-end cold-start** = `submit_ms` average under `sandbox=docker`.
  Dominated by CP tick cadence (≈1 s) + node poll interval, so any sandbox
  delta is strictly its *additional* cost, not the raw container start.
- **Raw container start** = delta between the two modes when the only change
  is the node-side `AGENTGRID_SANDBOX` flag. On modern kernels with a warm
  overlay backing this is typically 100–500 ms for a static `alpine`-size
  image; the agentgrid image is larger, so budget 1–2 s.
- **Podman** routes through the same `docker run` CLI (alias) — the
  benchmark measures it identically once the daemon runs under Podman.
  microVM (Firecracker) is future work (roadmap #9 follow-up).
- **Pre-warmed pool:** if the observed delta exceeds the 5 s threshold the
  plan sets, add a `SandboxPool` to the node daemon that keeps N stopped
  containers primed with the sandbox image (`docker create` without start)
  and hands them out on spawn. That is **conditional** on the benchmark
  result — do not build it until the cold-start mat it.

## Hand-measured baseline (reference environment)

Local quick check on the development host (Docker 29.3, `alpine:3.20`,
warm pull):

| mode | time |
|------|------|
| `docker run --rm alpine true` (first) | ~1.77 s |
| same, subsequent runs | ~0.9 s |
| same + `--network none --read-only --cap-drop=ALL` | ~0.46 s |

Conclusion for this hardware: the container-management overhead is under the
plan's 5 s threshold even without a pre-warm pool — the pre-warm pool is
**conditional**, only built if a heavier production image pushes the delta
past 5 s. Pulling the sandbox image fresh (multi-GB layers) is the dominant
first-run cost, so always `docker pull` before benchmarking.

## What is NOT measured

- Task teardown time (`docker rm -f` on completion/kill).
- Concurrency effects (two sandboxes racing the node's single executor).
- Egress allow-list extra hop, if a nftables sidecar is in play.

If the bench shows `submit_ms` avg ≥ 5 s for the sandbox path, the next
step is the pre-warm pool design, not more benchmarks.
