//! OpenTelemetry metrics and traces (optional feature).
//!
//! This module provides the OpenTelemetry pipelines when the `opentelemetry`
//! feature is enabled. When the feature is disabled, `init_otel` is a no-op and
//! `init_traces` does not exist, so the default build keeps the dependency
//! graph it had.

#[cfg(feature = "opentelemetry")]
use anyhow::Context;
use anyhow::Result;

/// A resource with the configured service name (`OTEL_SERVICE_NAME`).
#[cfg(feature = "opentelemetry")]
fn resource() -> opentelemetry_sdk::Resource {
    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .unwrap_or_else(|_| "agentgrid-control-plane".to_string());
    // otel 0.32: Resource::new went pub(crate); the builder is the public API.
    opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name)
        .build()
}

/// Initialize OpenTelemetry metrics pipeline with Prometheus exporter.
///
/// Configuration via environment variables:
/// - `OTEL_EXPORTER_PROMETHEUS_ENABLED`: set to "true" to enable Prometheus exporter (default: false)
/// - `OTEL_SERVICE_NAME`: service name (default: "agentgrid-control-plane")
///
/// Traces are separate (`init_traces`) because a span exporter has to be part
/// of the subscriber, which only `main` can build.
#[cfg(feature = "opentelemetry")]
pub fn init_otel() -> Result<()> {
    // Build the meter provider with Prometheus reader (if enabled)
    let mut builder =
        opentelemetry_sdk::metrics::SdkMeterProvider::builder().with_resource(resource());

    if std::env::var("OTEL_EXPORTER_PROMETHEUS_ENABLED").unwrap_or_default() == "true" {
        // Create Prometheus registry
        let registry = prometheus::Registry::new();

        // Build the Prometheus exporter with the registry
        let prom_exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .context("failed to build Prometheus exporter")?;

        // Add the Prometheus exporter as a reader
        builder = builder.with_reader(prom_exporter);
        tracing::info!("OpenTelemetry Prometheus exporter enabled");
    }

    let provider = builder.build();
    opentelemetry::global::set_meter_provider(provider);

    tracing::info!("OpenTelemetry metrics initialized");
    Ok(())
}

/// The trace provider, when OTLP export is configured.
#[cfg(feature = "opentelemetry")]
pub type TraceProvider = opentelemetry_sdk::trace::SdkTracerProvider;

/// Start OTLP trace export if `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
///
/// Returns the provider, which the caller must keep alive for the process
/// lifetime and shut down at exit — dropping it without `shutdown` loses the
/// spans still queued in the batch processor, which is the difference between
/// "traces are exported" and "traces are usually exported".
///
/// The endpoint is read from the standard OTLP variable rather than a bespoke
/// one, so an operator's existing collector config works unchanged. Unset means
/// no export and no extra work: the default deployment has no collector, and
/// ADR 0025 was right that a pipeline nobody runs is dead code — this one is
/// opt-in, and it is verified by a test that runs a receiver.
#[cfg(feature = "opentelemetry")]
pub fn init_traces() -> Result<Option<TraceProvider>> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").unwrap_or_default();
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Ok(None);
    }
    let provider = build_trace_provider(endpoint)?;
    tracing::info!(endpoint, "OpenTelemetry OTLP trace export enabled");
    Ok(Some(provider))
}

/// Build (and install as global) a trace provider that exports to `endpoint`.
///
/// Split out from `init_traces` so a test can point it at a local receiver
/// without mutating the process environment.
#[cfg(feature = "opentelemetry")]
fn build_trace_provider(endpoint: &str) -> Result<TraceProvider> {
    // `with_endpoint` takes the URL verbatim — it does not append the signal
    // path the way the `OTEL_EXPORTER_OTLP_ENDPOINT` env var does. Passing the
    // base URL alone made the exporter POST to `/`, which a collector answers
    // with 404: the test caught it before a deployment did.
    let traces_url = format!("{}/v1/traces", endpoint.trim_end_matches('/'));
    let exporter = {
        // `with_endpoint` lives on the `WithExportConfig` trait, not on the
        // builder type — the compiler points at the builder, not the missing
        // import.
        use opentelemetry_otlp::WithExportConfig;
        opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(traces_url)
            .build()
            .context("failed to build the OTLP span exporter")?
    };
    let provider = TraceProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok(provider)
}

/// A `tracing` layer that turns spans into OTLP spans.
#[cfg(feature = "opentelemetry")]
pub fn trace_layer<S>(
    provider: &TraceProvider,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use opentelemetry::trace::TracerProvider as _;
    tracing_opentelemetry::layer().with_tracer(provider.tracer("agentgrid-control-plane"))
}

/// ADR 0025 deferred this feature because there was no collector to verify
/// against — "unverified telemetry is worse than none". So the test brings its
/// own receiver: a one-shot HTTP server on a loopback port, and an assertion
/// that a span actually left the process and landed on `/v1/traces` as a
/// non-empty protobuf body.
#[cfg(all(test, feature = "opentelemetry"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Reads one HTTP request and answers 200. Returns the endpoint and a
    /// handle yielding (request line, bytes received).
    fn spawn_receiver() -> (String, std::thread::JoinHandle<(String, usize)>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind receiver");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .expect("timeout");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                // Headers, then exactly Content-Length body bytes.
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                    let want = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + want {
                        break;
                    }
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            let first = String::from_utf8_lossy(&buf)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            (first, buf.len())
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn a_span_reaches_an_otlp_endpoint() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let (endpoint, receiver) = spawn_receiver();
        let provider = build_trace_provider(&endpoint).expect("trace provider");

        // Another test may have installed a subscriber already; `init` would
        // panic there. What matters is that *this* subscriber records the span.
        let layer = trace_layer(&provider);
        let _ = tracing_subscriber::registry().with(layer).try_init();

        tracing::info_span!("attempt", task = "t1").in_scope(|| {
            tracing::info!("phase: agent");
        });

        // Flushes the batch processor. Without it the span is still queued and
        // the assertion below would fail — which is the point of calling it out.
        provider.shutdown().expect("shutdown flushes");

        let (request_line, bytes) = receiver.join().expect("receiver");
        assert!(
            request_line.contains("/v1/traces"),
            "unexpected request: {request_line}"
        );
        assert!(
            bytes > 100,
            "OTLP body was {bytes} bytes — headers only, no protobuf"
        );
    }
}

/// No-op when the `opentelemetry` feature is disabled.
#[cfg(not(feature = "opentelemetry"))]
pub fn init_otel() -> Result<()> {
    Ok(())
}
