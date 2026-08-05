//! OpenTelemetry metrics initialization (optional feature).
//!
//! This module provides OpenTelemetry metrics setup when the `opentelemetry`
//! feature is enabled. When the feature is disabled, `init_otel` is a no-op.

#[cfg(feature = "opentelemetry")]
use anyhow::Context;
use anyhow::Result;

/// Initialize OpenTelemetry metrics pipeline with Prometheus exporter.
///
/// Configuration via environment variables:
/// - `OTEL_EXPORTER_PROMETHEUS_ENABLED`: set to "true" to enable Prometheus exporter (default: false)
/// - `OTEL_SERVICE_NAME`: service name (default: "agentgrid-control-plane")
///
/// Note: OTLP exporter is intentionally omitted to avoid heavy dependencies.
/// Users can add OTLP via their own exporter if needed.
#[cfg(feature = "opentelemetry")]
pub fn init_otel() -> Result<()> {
    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .unwrap_or_else(|_| "agentgrid-control-plane".to_string());

    let resource = opentelemetry_sdk::Resource::new(vec![opentelemetry::KeyValue::new(
        "service.name",
        service_name,
    )]);

    // Build the meter provider with Prometheus reader (if enabled)
    let mut builder =
        opentelemetry_sdk::metrics::SdkMeterProvider::builder().with_resource(resource);

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

/// No-op when the `opentelemetry` feature is disabled.
#[cfg(not(feature = "opentelemetry"))]
pub fn init_otel() -> Result<()> {
    Ok(())
}
