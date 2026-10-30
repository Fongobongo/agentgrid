# ADR 0025: OTLP traces — split into phase timings (now) and export (deferred)

Status: spike evaluated (backlog item 6) — **6a accepted, 6b deferred**

## Context

`crates/control-plane/src/otel.rs` initialises a `SdkMeterProvider` with an
optional Prometheus reader and says in its own doc comment that the OTLP
exporter is "intentionally omitted to avoid heavy dependencies". There is no
span per attempt or per step, so a slow attempt cannot be decomposed: the only
timing that exists is `agentgrid_task_duration_seconds`, a histogram over
*whole* task durations (`recent_terminal_task_seconds`).

Two facts change what "add OTLP" costs:

1. **The whole `opentelemetry` feature is off by default**
   (`crates/control-plane/Cargo.toml`: `default = []`), so `init_otel` is a
   no-op in every shipped binary. Adding an OTLP exporter to a pipeline nobody
   builds does not produce traces; it produces a second dead code path.
2. **`tracing` is not wired to any subscriber registry.** Every binary calls
   `tracing_subscriber::fmt().init()` — a formatter, not a `Registry` — so an
   OTel layer cannot simply be added; initialisation has to be restructured in
   `control-plane/src/main.rs` (and, for node-side spans, in four more
   binaries).

## What 6b would cost

- `tracing-opentelemetry` (0.34 is current; whether it pairs with our
  `opentelemetry` 0.33 is *unverified* — the crates are versioned one apart by
  convention, not by contract) and `opentelemetry-otlp` with `http-proto`
  (`prost`, no tonic — `reqwest` is already a dependency).
- Restructuring tracing initialisation into `registry().with(fmt_layer)
  .with(otel_layer)` in the binaries that should emit spans.
- A feature-flag decision: keep `opentelemetry` opt-in and nobody gets traces
  unless they rebuild the CP, or make it default and accept the dependency
  graph change for the musl/static release targets.
- A verification story. There is no collector in CI and none on the test hosts,
  so the only thing that can be checked locally is "it compiles" — unverified
  telemetry is worse than none, because an operator will trust it.

## Decision

- **6a (accepted): phase timings.** Instrument the attempt lifecycle so the
  phases are separately measurable (clone → agent → validation → push) and
  expose them as a labelled histogram plus an attempt event. No new dependency,
  verifiable in tests and in `tests/e2e`, and it answers the question the
  operator actually has ("where did the 40 minutes go?") without a collector.
- **6b (deferred): OTLP export.** Revisit when there is a collector to verify
  against, or when the `opentelemetry` feature becomes default for another
  reason — at which point the layer restructure is a small, testable change
  instead of a speculative one.

## Revisit when

A collector exists in the deployment (`deploy/` has no otel-collector today),
or a user reports wanting to ship spans to an existing APM.
