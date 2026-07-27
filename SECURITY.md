# Security Policy

## Reporting a Vulnerability

AgentGrid is pre-1.0 software under active hardening. We welcome security
reports.

- **Report privately** by opening a private security advisory on the GitHub
  repository (Security tab → "Report a vulnerability"), or email the
  maintainers directly.
- Do **not** open a public issue for a suspected vulnerability.
- Include: affected version, component (control-plane / node-daemon / CLI /
  web UI), a minimal reproduction, and your assessment of impact.
- You will receive an acknowledgment within 5 business days.

## Scope

The threat model lives in
[`docs/decisions/threat-model.md`](docs/decisions/threat-model.md). In
summary, AgentGrid assumes:

- The control plane is the trust root. A node is only trusted for work it was
  assigned to; cross-node access is rejected at the API boundary.
- Node-to-control-plane traffic should be TLS-terminated; the daemon supports
  rustls TLS and validates the server identity.
- Agent worktrees are **not** a security sandbox. Run untrusted agents with
  the Docker/Podman sandbox backend and a restrictive network/secrets policy
  (see `docs/decisions/threat-model.md`). Do not rely on filesystem isolation
  for hostile code.
- `--dangerously-skip-permissions` style adapter bypasses require an explicit
  `AGENTGRID_UNSAFE_UNATTENDED=1` opt-in; the default is fail-closed.

## Supported Versions

Only the latest released version receives security fixes. There are no
backport branches before 1.0.
