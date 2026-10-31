# ADR 0028: Per-host credentials — what a vault would and would not buy

Status: accepted — Phase 0 shipped, Phase 1 chose **B** (node-held keypair) and
is implemented (both unreleased, 0.7.0); Phase 2 (install credentials) and
Phase 3 (node credential rotation) are still open. See [Outcome](#outcome).

## Context

ADR 0027 closed with one sentence still open:

> **Credentials need to be per-host.** Today one key path or one agent serves
> every install. Per-host keys mean a vault, and a vault means deciding what
> the control plane is allowed to decrypt.

That sentence conflates three secrets with three different blast radii. "Do we
need a vault?" is really three questions, and the answer differs per row.

### What exists today

| Secret | Where it lives | Lifetime | Blast radius if leaked |
|---|---|---|---|
| Node credential | `<node-data>/credential.json`, mode 0600, plaintext JSON (`enrollment.rs`) | forever — no rotation path | one node |
| Adapter keys, CP-managed | `adapter_env.value` in SQLite, plaintext; pushed to nodes in every poll response | forever | the provider account, fleet-wide |
| Adapter keys, node-local | node env file / `AGENTGRID_ADAPTER_ENV` | forever | one node |
| Install SSH key or password | request body only, never persisted (`crates/provision`) | one install | every host that key reaches |
| JWT signing secret | control-plane env | forever | every session |
| Node credential, CP side | `nodes.credential_hash` (sha256 hex) | forever | nothing — it is a hash |

The CP-managed adapter env is already per-host capable: `adapter_env.node_id`
scopes an entry to one node, `''` means global, and `adapter_env_for()` returns
globals first so a node-scoped row can override a fleet default. The *mechanism*
for per-host credentials exists. What does not exist is any protection around
it.

## What I measured before proposing anything

Four claims below are load-bearing, so they were executed against a real
control plane (a throwaway DB on `127.0.0.1`, the same binaries the release
ships), not read off the source.

**1. A view-only role can read every secret in plaintext.** `operator_allowed`
in `crates/control-plane/src/auth.rs` lets every `GET` through, and
`list_adapter_env` returns `value` as stored:

```
PUT /v1/adapter-env as operator          -> 403     (RBAC works)
GET /v1/adapter-env as operator          -> 200
{"items":[{…,"key":"ANTHROPIC_API_KEY",
           "value":"sk-ant-SUPERSECRET-do-not-read"}]}
```

So the role that exists to be *safe to hand to a teammate* can exfiltrate the
fleet's provider keys with one request. `ag adapter-env ls` prints them too.

**2. The values are plaintext at rest.** `grep -c` on the live database:
`cp.db-wal` = 1 hit, `cp.db` = 0 (not yet checkpointed). A copied DB, a backup,
a disk image, or a `.db` sitting in a support ticket is the whole fleet's keys.

**3. Deleting an entry is not erasure.** After `DELETE FROM adapter_env` and a
`wal_checkpoint(TRUNCATE)`, the file still contains the value (1 hit); only
`VACUUM` removes it (0 hits). So `ag adapter-env rm` — the only revocation verb
we have — leaves the secret readable in the file it was removed from.

**4. CP-pushed values are not masked in agent output.** `cfg.secrets` comes
from `AGENTGRID_SECRETS` only; `managed_adapter_env` values are injected into
the attempt's env (`attempt_runner`) but never added to the redactor's list. An
adapter that echoes its env into stdout leaks a key the CP *knew* was a key,
and nothing on the node was told to hide it.

## The three problems, which are not one problem

- **(a) A stolen file.** Backup, disk image, `scp` of the DB. Encryption at
  rest fixes this, and only encryption at rest fixes it.
- **(b) A compromised control plane.** The CP must hand secrets to nodes, so it
  must be able to read them. **No CP-side vault fixes this.** Not Vault, not
  KMS, not envelope encryption. Two designs change (b) at all: the secret never
  reaches the CP in plaintext (node-held key), or the secret is short-lived and
  minted per attempt by the provider. Everything else is (a) with better
  marketing.
- **(c) Accountability.** "Who read which key, when." Requires the read path to
  be narrow enough to audit; today it is `GET` for anyone.

A proposal that claims to solve (b) with a vault is wrong, and the ADR says so
before someone buys a license on that basis.

## Options

| | What it is | Fixes | Costs | Fails at |
|---|---|---|---|---|
| **A** | Envelope encryption: AES-256-GCM per value, KEK from outside the DB (`AGENTGRID_SECRET_KEY`, 32 bytes, or a file), fail-closed if the KEK is missing while encrypted rows exist | (a), and (c) once the read path is narrowed | one migration, one dep (`aes-gcm`), key distribution to one env var | (b) — the CP still decrypts |
| **B** | Node-held keypair: each node generates X25519 at enroll and sends the public key; the CP stores **only** ciphertext addressed to that node and can never read it | (a) **and** (b) for reading | enrollment protocol change, N ciphertexts per global secret, a re-key path, a fallback for nodes that never sent a key | (b) for *use* — a compromised CP can still push a config that exfiltrates |
| **C** | External secret manager as source of truth: entries store `vault://path#key`, the CP resolves at push time, caches in memory | (a) and rotation/audit delegated to a tool ops already run | a new service to deploy, a credential for the CP itself, a resolver trait | (b) — the CP holds plaintext at push time |
| **D** | Document the risk, change nothing | nothing | nothing | all three |

Two shortcuts that look like solutions and are not:

- **Key stored next to the ciphertext** (same dir, same env file, same SQLite
  row). Protects against `cp.db` being copied without the key, and nothing
  else. This is where most home-grown "vaults" land.
- **Encrypt the poll payload with the node credential.** Impossible as stated:
  the CP stores `credential_hash` (sha256), so it cannot derive a key. Making
  it possible means storing the credential itself, which is strictly worse.
  B is the version of this idea that works, and it needs a real public key.

## Decision (proposed)

Ship in this order. Only the first is unconditional.

**Phase 0 — stop leaking (no policy decision needed).**

1. `GET /v1/adapter-env` returns a **fingerprint**, never the value:
   `sha256(value)[:8]` plus length, e.g. `sk-…·a91f3c2e·51`. The list becomes
   write-only, which is what a secret store is.
2. `GET /v1/adapter-env` is admin-only — one clause in `operator_allowed`. An
   operator has no reason to enumerate the fleet's keys.
3. `ag adapter-env ls` prints the fingerprint; `ag adapter-env set` reads the
   value from stdin when the argument is omitted, so it stops landing in shell
   history.
4. `ag adapter-env rm` runs `VACUUM` (or `PRAGMA secure_delete=ON` for the
   session) so removal is erasure, per measurement 3.
5. CP-pushed values are added to the node's mask list automatically, so
   `AGENTGRID_SECRETS` stops being a per-node manual duplicate of something the
   CP already knows is a secret.

**Phase 1 — pick A or B.** A is ~a day and closes (a). B is ~a week, closes
(a) and the *read* half of (b), and needs a protocol bump. C is worth doing
only if the operator already runs Vault or a cloud KMS; it is a resolver trait
plus one implementation, not an architecture.

**Phase 2 — install credentials (ADR 0027's T35).** Do not store, and do not
accept a long-lived key: `InstallRequest` grows `ssh_cert` + an ephemeral key,
the operator's CA signs, the certificate expires. One key for the whole fleet
is the worst row in the table above and the one an install button makes
routine.

**Phase 3 — node credential rotation.** `ag nodes rotate <id>`: issue a new
credential, accept both until the node's next successful poll, then drop the
old. Lowest blast radius of the set, so last — but it is trivial now and
awkward once nodes have been enrolled for a year.

## Consequences

- Phase 0 is a **breaking API change**: anything that reads `value` from
  `GET /v1/adapter-env` gets a fingerprint instead. Nothing in the repo does
  (the web UI never calls it; the node gets values through the poll response,
  not this route), so the break is theoretical — but it is a break, and it
  belongs in a minor release with a changelog line, not a patch.
- Phase 1A makes the CP refuse to start without its KEK when encrypted rows
  exist. That is deliberate: falling back to plaintext on a missing key is the
  downgrade bug that turns a vault into decoration.
- Neither phase stops an agent from exfiltrating a key it was legitimately
  given. That is what `AGENTGRID_SECRETS` masking and ADR 0023's no-go on
  per-task egress are about, and it stays unsolved here.
- If the answer to Phase 1 is B, the CP's ability to "decrypt" becomes a
  property of the nodes, and a lost node key means a secret must be re-entered
  — an operational cost worth stating before it is chosen, not after.

## Outcome

**Phase 0** shipped as written: the list route returns `fingerprint` and
`value_len` and has no field able to hold a value; it is admin-only; `rm` sets
`secure_delete` and truncates the WAL; CP-pushed values join the node's mask
list automatically (`is_secret_name` — `_KEY`, `_TOKEN`, `_SECRET`,
`_PASSWORD`, `_PASS`, `_PAT`). Verified on a live control plane: the operator
role gets 403 on `GET` *and* `PUT`, and after `rm` the secret is `grep`-able in
neither `cp.db` nor `cp.db-wal`.

**Phase 1 = B**, implemented as ECIES over X25519 in `crates/common/src/seal.rs`:

- The node generates a static X25519 keypair at enroll and stores the private
  half in `credential.json` (0600). The public half is sent in `EnrollRequest`
  and re-sent on every heartbeat, so a node enrolled before this change
  becomes sealable on upgrade without re-enrolling.
- The CP seals each value **per node** with a fresh ephemeral key
  (HKDF-SHA256, salt `agentgrid-adapter-env-v1`, AES-256-GCM) into
  `adapter_env_sealed`, and keeps only `sha256(value)[:8]` and the length on
  the entry itself. `adapter_env.value` was renamed to `value_legacy`, blanked
  by a startup step (`seal_legacy_adapter_env`) once node keys exist, and never
  written again.
- The entry identity is bound **into the derived key**, not only into the tag:
  `aad = node_id \0 adapter \0 key`. A blob replayed as another variable or for
  another node fails to open rather than decrypting to the wrong entry.
- A node that has not announced a key yet gets nothing and shows up as
  `pending` in `ag adapter-env ls`, instead of being handed a blob it cannot
  use. Announcing a *different* key drops the blobs sealed to the old one.
- `NODE_PROTOCOL_VERSION` went `1` → `2`: `adapter_env` in a poll response now
  carries `sealed` instead of `value`. A protocol-1 node is served no
  CP-managed adapter env at all, which is the only honest thing to do — it
  would otherwise receive ciphertext it treats as the value.
- `PollResponse.adapter_env_complete` (new) says the list is the node's whole
  set, not a delta. Without it an empty list is ambiguous and `rm` could not
  revoke a value on a running node — only at its next restart.

What B does **not** buy: a compromised CP can still *push* a value of its
choosing (it seals, it does not sign) and can still point an adapter at a base
URL it controls. It cannot read back what an operator entered. That is the
(read-half) claim the ADR made for B, and it holds.

**Operational cost, stated before it is discovered:** upgrading a node mints a
key it did not have, so any value sealed before the upgrade is unrecoverable —
those entries report `pending` and must be re-entered once. Rotating a node's
key has the same cost for that node.

`tests/e2e/run-sealed-env.sh` (CI) proves the wiring end to end: a value piped
into `ag adapter-env set` is sealed for the node, is absent from `cp.db`,
`cp.db-wal` and the CP log, reaches the adapter process (the mock prints what
the child saw), is masked in the event stream while a non-secret variable is
not, and is gone from the next attempt after `ag adapter-env rm`.

## Method

```bash
# 1. view-only role reads plaintext
curl -X POST $BASE/v1/users -H "authorization: Bearer $admin" \
  -d '{"username":"viewer","password":"…","role":"operator"}'
curl -X PUT $BASE/v1/adapter-env -H "authorization: Bearer $viewer" … # 403
curl     $BASE/v1/adapter-env -H "authorization: Bearer $viewer"      # 200 + value

# 2/3. at rest, and whether DELETE erases
grep -c SUPERSECRET cp.db-wal          # 1 before checkpoint, 1 in cp.db after
sqlite3 cp.db 'DELETE FROM adapter_env'
grep -c SUPERSECRET cp.db              # 1  — deletion is not erasure
sqlite3 cp.db 'VACUUM'
grep -c SUPERSECRET cp.db              # 0
```
