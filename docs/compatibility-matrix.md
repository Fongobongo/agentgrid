# OS / filesystem compatibility matrix

> Plan 6.13 / 6.14 follow-up: documents what we test, what works by
> construction, and what is honest best-effort. The enforced constraint
> (README "Hard constraints"): Linux only, kernel >= 5.10, no 32-bit,
> no big-endian, no SQLite/workspaces on NFS or network filesystems.

## Tier 1 — full support

| OS | CI | Notes |
|---|---|---|
| Ubuntu 24.04 LTS x86_64 | `ci.yml` (rust/web/e2e jobs run on `ubuntu-latest` ≈ 24.04) | Primary dev + release host. |
| Debian 12 (bookworm) x86_64 | nightly `tier1-musl-smoke` job: musl binaries `--version` + CP `/health/ready` inside `debian:12` container | musl static → glibc version independent. Verified manually on a remote box (`docs/deploy/remote-smoke-v0.1.md`). |
| Debian 13 (trixie) x86_64 | nightly `tier1-musl-smoke` job | Same musl path. |
| systemd + Git >= 2.39 | assumed by `deploy/` units + `install-node.sh` | Hard constraint; the daemon works without systemd too (plain process), only the shipped unit files assume it. |

Filesystems: **ext4** is what every CI runner and lab host uses (implicit
coverage). **xfs** is expected to work identically (no ext4-specific
ioctls anywhere; artifact writes are plain `write_all` + `fdatasync`),
but has no dedicated CI — treat as supported-by-construction, not tested.

## Tier 2 — published binaries, smoke-tested

| OS | CI | Notes |
|---|---|---|
| ARM64 Ubuntu/Debian | nightly `arm64-musl-smoke` (QEMU): `--version` on all three main binaries | The full mock happy-path on real ARM hardware is still follow-up (needs an ARM runner). |
| Fedora / Rocky / Alma / Arch | **not tested** | musl static binaries should run anywhere with kernel >= 5.10; sandbox Docker path requires a Docker-compatible runtime. No CI, no manual matrix yet — contributions welcome. |

## Tier 3 / best effort (documented, unsupported)

- **WSL2** — practical and lab-tested (`deploy/deploy-termux.md` /
  CHANGELOG WSL2 notes). Two rules:
  1. Run the daemon **inside the WSL2 distro**, never on `/mnt/c` (the
     9P bridge breaks file locking and `O_NOFOLLOW` semantics).
  2. Keep every attempt worktree under the distro's ext4 (`~/...`).
     The shipped systemd sandbox needs the documented drop-in that
     relaxes `ProtectSystem/PrivateTmp` (the WSL2 VM boundary already
     provides the isolation those directives exist for).
- **Windows hosts / WSL1** — not supported at all as node/CP targets.
  Windows is supported only as a *development* host (the workspace
  compiles and the pure unit tests run; unix-shell tests are
  `#[cfg(unix)]`-gated).
- **Alpine** — musl binaries are built FOR this world, but no CI runs
  on Alpine itself. Docker images (`Dockerfile.control-plane-musl`,
  `FROM scratch`) are the supported Alpine-adjacent path.
- **NixOS / no-systemd / NAS / read-only root** — nothing is tested;
  the daemon itself does not require systemd (plain process), but the
  deploy units and `install-node.sh` assume it. Read-only `/` needs
  `AGENTGRID_DATA_DIR` / workspaces redirected to a writable volume.

## What "musl" buys you

All release binaries are statically linked musl builds: they do not
depend on the host glibc, so the same tarball runs on Ubuntu 24.04,
Debian 12/13, Fedora, Rocky, Alpine, and WSL2 alike. The GNU
(`x86_64-unknown-linux-gnu`) fallback exists for hosts where musl
misbehaves with DNS/proxy (see TROUBLESHOOTING.md "TLS / DNS" if you hit
`getaddrinfo`-style failures).
