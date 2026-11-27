# Release process

## Branch and tag flow

1. `master` is the integration branch. Every push runs CI: format, clippy,
   workspace tests, image builds, E2E, and the backup/restore rehearsal.
2. A release candidate lives on a short-lived `release/<version>` branch
   created from `master`. RC branch pushes run the same CI gates. Cherry-pick
   only reviewed fixes; never build release artifacts from an untested local
   tree.
3. When the RC branch is green, fast-forward or merge it into `master`, then
   create the signed tag on that `master` commit:
   `git tag -s vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`.
4. The tag push runs `.github/workflows/release.yml`: cross-built musl/gnu
   binaries, host-target workspace tests, UI build, glibc ceiling check,
   binary-size guardrail, RSS budget, checksums, SBOM, provenance
   attestation, cosign signature, GitHub Release, and GHCR image.
5. `release/<version>` branch is deleted after the release is published.

## Why tag-only publication

`release.yml` publishes images and a GitHub Release only for `refs/tags/v*`.
A branch build may upload CI artifacts for inspection, but it cannot move
`:latest` or create a public release. This keeps an unmerged or unreviewed
branch from shipping binaries.

The release job refuses a tag whose version differs from
`[workspace.package].version` in `Cargo.toml`. Bump every version pin in the
same commit that creates the tag.

## Restore drill

Before promoting a release candidate, run the backup/restore rehearsal:

```bash
cargo build --workspace
bash tests/e2e/run-restore.sh
```

The drill starts one control plane and node, runs a task, creates an online
backup, tears the stack down, restores into a fresh database on another port,
asserts the old task and its status survived, enrolls a fresh node, and runs a
new task successfully. CI runs the same script on every push and nightly.

## Runtime smoke after deployment

After deploying a release:

```bash
AGENTGRID_DB=/var/lib/agentgrid/control-plane.db ag doctor runtime
curl -fsS http://127.0.0.1:7800/health/ready
```

`ag doctor runtime` reports database/WAL paths, instance-lock inode and
holder, free disk, deleted-open file descriptors, and stale temp dirs. A
deleted-open descriptor is a failure: some process is still holding bytes
that no longer appear in `du`.
