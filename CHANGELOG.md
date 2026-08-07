# Changelog

All notable changes to this project are documented in this file.

## [v0.3.1](https://github.com/earendil-works/agentgrid/tree/v0.3.1) - 2026-08-07

### Added (0.3 pass — final release artifacts)

- **Musl static-link release binaries:** Dockerfile.control-plane-musl produces
  `x86_64-unknown-linux-musl` control-plane + CLI, fully self-contained with web UI.
  Image `ag-cp:musl` = 11MB, no libc dependencies, runs anywhere on Linux x86_64.

- **Transport selection runbook:** docs/runbook-transport.md explains poll vs WS
  tradeoffs, deployment guidance, monitoring metrics, testing commands, and load
  baseline numbers (100-node harness: wall=30.4s, p50=21.3s, write_lock_failures=0).

### Operator Documentation

- **OPS-STARTER.md:** Quick deployment guide for Docker Compose and systemd installations,
  monitoring setup, backup procedures, and common operations.
- **TROUBLESHOOTING.md:** Comprehensive troubleshooting guide for transport issues, resource
  constraints, task execution failures, disk problems, WebSocket-specific issues, performance
  optimization, security concerns, and disaster recovery procedures.

### Metrics & Baselines

- **CP idle RSS:** 4 MiB VMRSS (well under 96 MB budget)
- **Load baseline (100 nodes):** 30.4s wall time for 1000 tasks, p50=21.3s assign latency,
  p99=29.5s, write_lock_failures=0 across both `poll` and `ws` transports
- **E2E verification:** All four transport combinations pass (`run.sh` + `run-two-host.sh`,
  both `AGENTGRID_TRANSPORT=ws` and `poll`)

### Changed (deployment fixes)

- Fixed AGENTGRID_JWT_SECRET export chain in `deploy/compose/up.sh` so compose
  interpolation succeeds reliably
- BuildKit cache-mount fix in Dockerfiles: copy to /out inside same RUN to hide binaries
  from subsequent COPY --from layers

See also:
- docs/plans/0.3-websocket-and-scale.md — full plan with stage-by-stage breakdown
- docs/load-baseline-0.3.md — detailed performance analysis and reproduction steps
- docs/node-ws-protocol.md — WebSocket protocol specification


## [v0.3.0](https://github.com/earendil-works/agentgrid/tree/v0.3.0)

*Legacy tag — superseded by v0.3.1 which includes all features plus final documentation and musl build.*

WebSocket channel implementation completed per plan 0.3 stage 2:

- ADR 0009 + node WS protocol spec
- CP endpoint `/v1/node/ws` with Bearer auth, hello/hello_ok handshake
- Node-daemon WS client (tokio-tungstenite/rustls, no OpenSSL)
- WS resilience: reconnect backoff, fencing tokens on ack path, cancel propagation
- Transport selection: `AGENTGRID_TRANSPORT=ws|poll|auto` with fallback
- E2E tests for both transports green
- Failure injection test: CP kill mid-attempt survives with durable outbox
- Initial Stage 3 load baseline measurements (Stage 3.1–3.2 finalized in v0.3.1)
