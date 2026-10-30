# ADR 0024: Decode-and-rescan in the skill scanner

Status: accepted (backlog item 3 of `docs/ROADMAP.md`)

## Context

`crates/skills/src/scanner.rs` is a static regex catalog over the text of a
skill bundle or MCP server manifest. It catches the payloads that are
*spelled out*: `ignore previous instructions`, `curl … | bash`, webhook sinks,
`cat ~/.aws/credentials`. It cannot see the same payload when it is shipped
encoded, and encoding is the cheap evasion: a `SKILL.md` can carry a
base64 blob that the agent — or any tool the skill drives — decodes at run
time, and the scanner reports "no findings".

The catalog already has `shell_base64` and `data_uri_payload` patterns, which
match the *shape* of a decode pipeline (`… | base64 -d | sh`), not the decoded
content. A bare blob with no pipeline around it is invisible.

## Decision

`scan_content` decodes every encoded-looking run in the document and runs the
same catalog over the decoded text, tagging each hit `<pattern>_encoded`,
naming the encoding in `detail`, and reporting the **offset of the blob in the
original document** — a reviewer is pointed at something they can find in the
file, not at a synthetic position inside a string that does not appear there.

Three encodings are peeled: base64 (standard and URL-safe, padding optional),
hex, and percent-encoding. Decoded bytes must be valid UTF-8 and ≥ 90 %
printable to be rescanned: binary noise cannot carry an instruction, and
without that filter a long identifier run would produce a full catalog pass
over garbage.

Bounds are part of the contract, because the scanner runs on the registration
path:

| bound | value | why |
|---|---|---|
| shortest run considered | 24 bytes (12 for percent) | shorter cannot carry an instruction |
| largest decoded payload | 64 KiB | bigger is a data file, not a payload |
| blobs per document | 32 | each one costs a full catalog pass |
| layers peeled | 2 | base64-in-base64 is real; unbounded recursion is a DoS |

`b64_decode`/`hex_decode`/`pct_decode` are hand-written (the crate has no
base64 dependency and the module is deliberately dependency-free apart from
`regex`/`serde`); each is ~20 lines and unit-tested through the fixtures.

## Consequences

- A payload hidden as a bare encoded blob is now reported as `Critical` with
  the encoding named, instead of passing silently.
- False positives stay low by construction: a benign encoded example
  (`base64("hello world, this is a test")`) decodes to text with no catalog
  match and produces no finding — asserted as a test.
- The bound on layers is a documented limit, not a bug: a payload encoded
  three times is not found. The test pins that behaviour so a future change to
  the limit is a deliberate decision.
- Cost is bounded: at most 33 catalog passes per document (1 plain + 32
  decoded), each over ≤ 64 KiB.

## Revisit when

A real evasion shows up in the wild that these three encodings do not cover
(e.g. gzip-then-base64, or a payload split across several blobs that only
makes sense concatenated) — the decode table is the single place to extend.
