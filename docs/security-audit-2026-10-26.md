# Security audit: 2026-10-26 (local, not release approval)

Base: `c982519f6eb51683e5c00356c71bc26ecf009e65` on master.
Read all nine open issue bodies via the public GitHub API and the current official
RustSec advisory-db records. Final database revision:
`a78282a1414d6c28cc228c4eae3570451affb0dc` (1378 advisories).
Primary source: https://github.com/RustSec/advisory-db/tree/a78282a1414d6c28cc228c4eae3570451affb0dc/crates

**Final status: all nine supplied issues (#79–#82, #97, #98, #102–#104) are
remediated.** Eight were already covered by the base lockfile; #103 was fixed
in this batch by migrating the JWT crypto backend (see below). `cargo audit`
and `cargo deny` both exit 0 with **no advisory ignores**.

## Issue mapping

These eight were already resolved in the base lockfile; no redundant upgrades:

| Issue | Advisory | Locked package | Current official patched ranges |
| --- | --- | --- | --- |
| #104 | RUSTSEC-2026-0386 | tokio 1.54.1 | >=1.51.6,<1.52.0 OR >=1.53.3 |
| #102 | RUSTSEC-2026-0385 | ctutils 0.4.5 | >=0.4.4 |
| #98 | RUSTSEC-2026-0381 | webpki-roots 1.0.11; shim 0.26.11 | >=1.0.10; shim =0.26.11 has no independent trust store |
| #97 | RUSTSEC-2026-0382 | rustls-webpki 0.103.16 | >=0.103.16,<0.104.0-alpha.1 OR >=0.104.0-alpha.8 |
| #82 | RUSTSEC-2026-0346 | rustls 0.23.46 | >=0.23.46 |
| #81 | RUSTSEC-2026-0349 | rustls 0.23.46 | >=0.23.46 |
| #80 | RUSTSEC-2026-0348 | rustls 0.23.46 | >=0.23.46 |
| #79 | RUSTSEC-2026-0347 | rustls 0.23.46 | >=0.23.46 |

`cargo tree --locked` confirms the shim resolves to webpki-roots 1.0.11, not
an affected parent. TLS packages are reached through reqwest/hyper-rustls,
tokio-rustls, tungstenite and the control-plane TLS configuration. The all-features
rustls graph does not enable brotli. No in-tree AsyncFd, EchMode or custom
ClientSessionStore usage was found. Patch versions, rather than reachability
exceptions, are the resolution evidence for these eight issues.

**#103 / RUSTSEC-2026-0387 is resolved in this batch** (the first pass of this
report found it open; the migration below landed afterwards). An in-place
`cargo update` was impossible: registry manifests pin jsonwebtoken's p256 to
^0.13.2, p384 to ^0.13.0 and elliptic-curve's crypto-bigint to ^0.5, so
`cargo update -p crypto-bigint --precise 0.7.6` exited 101 and the newest
0.7 patches still cannot satisfy ^0.5. The remediation is the alternative the
first pass named: **jsonwebtoken's `aws_lc_rs` backend.**

What changed:

- `Cargo.toml`: `jsonwebtoken = { version = "11", default-features = false,
  features = ["aws_lc_rs", "use_pem"] }` — was `features = ["rust_crypto"]`.
  The RustCrypto chain (p256/p384/ecdsa/elliptic-curve/crypto-bigint) and the
  unpatched `rsa` crate leave the graph entirely; `Cargo.lock` confirms none of
  `crypto-bigint`, `rsa`, `elliptic-curve`, `p256`, `p384` resolve anymore.
- The vulnerability ignores for RUSTSEC-2026-0387 and RUSTSEC-2023-0071 are
  **removed** from both `deny.toml` (`ignore = []`) and `supply-chain.yml` —
  a reintroduction must fail the gate.
- No new native-build burden: `aws-lc-sys` was already in the base lockfile
  through rustls' default aws-lc provider (tokio-rustls), so every target that
  built before (including the musl release image) already compiles it. The
  feature unification hazard is documented in `Cargo.toml` comments: never
  re-enable `rust_crypto`, including under workspace `--all-features`.
- JWT behavior is unchanged for our usage (HS256, `from_secret`,
  `Validation::default()` — crates/control-plane/src/lib.rs:309-324 and
  src/auth.rs:410-411): the AWS-LC provider implements HMAC. Runtime proof is
  the auth integration suite below (login issues a JWT, protected endpoints
  accept it, cookie auth round-trips).

## Additional real findings and fixes

During this audit the updated DB increased from 1355 to 1378 advisories.
Both cargo-deny and a refreshed cargo-audit found four additional compression
advisories, not represented by the nine supplied issue bodies:

- RUSTSEC-2026-0391: zstd 0.14.0, abandoned context stream data leakage;
  fixed range >=0.14.3.
- RUSTSEC-2026-0398: zstd-safe 8.0.0, seekable output length beyond capacity;
  fixed range >=8.2.0.
- RUSTSEC-2026-0403: zstd-safe 8.0.0, uninitialized output on legacy frame
  streaming decompression error; fixed range >=8.2.0.
- RUSTSEC-2026-0410: zstd-sys 2.1.0+zstd.1.5.7, wasm32 allocator memory
  corruption; fixed range >=2.1.2.

Used cargo update (no hand-edited lockfile): zstd -> 0.14.3, zstd-safe -> 8.2.0,
zstd-sys -> 2.1.2+zstd.1.5.7. Those three package entries/checksums changed; Cargo also re-resolved the
existing tempfile dependency's getrandom reference from 0.4.3 to 0.3.4 (both
versions were already locked). No additional package versions changed. Manifests remain compatible. These
packages are reached by control-plane directly and via async-compression /
compression-codecs / tower-http. No vulnerability ignores were added.

## Local verification actually performed

First pass (before the JWT backend migration):

- cargo audit --color never: exit 1, only crypto-bigint 0.5.5
  RUSTSEC-2026-0387 and rsa 0.9.10 RUSTSEC-2023-0071 remained. All eight
  resolved supplied advisories and four fixed compression advisories absent.
- cargo deny --color never check advisories licenses sources: exit 1 only on
  crypto-bigint; licenses and sources passed. RSA was the then-explicit
  exception.
- cargo check --locked -p zstd -p zstd-safe -p zstd-sys: PASS, exit 0,
  Windows host build completed in 13.63s.
- cargo fmt --all -- --check: PASS, exit 0.
- git diff --check: PASS, exit 0.
- Locked inverse graphs and all-features rustls graph inspected successfully.

Final pass (after the migration, current tree):

- cargo audit --color never: **exit 0**, 1378 advisories loaded, 456 locked
  dependencies scanned, zero findings, zero ignores.
- cargo deny --color never check: **exit 0** — "advisories ok, bans ok,
  licenses ok, sources ok" with `ignore = []`.
- Runtime authentication tests (the real auth suite lives in the `api`
  integration-test binary, `crates/control-plane/tests/api/`):
  - `cargo test -p agentgrid-control-plane --test api auth_approvals::` —
    **8 passed, 0 failed** (JWT setup/login protecting endpoints, login cookie
    round-trip, approval allow/deny/expiry, policy classification and
    autonomy levels).
  - `cargo test -p agentgrid-control-plane --test api security::` —
    **26 passed, 0 failed** (revoked node 401, cross-node isolation,
    login rate-limit 429, fencing tokens 409, artifact sha256/traversal,
    security headers, node-credential trust).
  These exercise the JWT login/verify path end-to-end under the AWS-LC
  provider — a genuine HS256 token is issued and accepted, not just compiled.

Not run locally in either pass: the full workspace test suite, MSRV build,
musl cross-build and Docker image builds — these are covered by CI on the
pushed commit. The 21.4GB workspace target stays pruned; no global tools were
installed.

Secondary existing fuzz run 40329643915 completed successfully on the unchanged
base SHA (public API status completed, conclusion success):
https://github.com/Fongobongo/agentgrid/actions/runs/40329643915
It does not verify these local compression updates or the JWT migration.

No force operation, credential persistence or unrelated deletion was performed.
Existing untracked .postman/ and postman/ were preserved. All nine issues have
remediation evidence recorded here; closure comments reference this report.
