//! `agentgrid-cred-gateway` — see `lib.rs` and ADR 0029.
//!
//! Configuration (all required except the listen address):
//!
//!   AGENTGRID_CRED_GATEWAY_ROUTES="anthropic|x-api-key|https://api.anthropic.com"
//!   AGENTGRID_CRED_GATEWAY_KEYS="anthropic=sk-ant-…"
//!   AGENTGRID_CRED_GATEWAY_ADMIN=<shared secret, same value the node daemon has>
//!   AGENTGRID_CRED_GATEWAY_LISTEN=0.0.0.0:7845   (default)

use std::sync::Arc;

use agentgrid_cred_gateway::{router, Gateway};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,agentgrid_cred_gateway=info".into()),
        )
        .init();

    let gateway = Arc::new(Gateway::from_env()?);
    let listen = std::env::var("AGENTGRID_CRED_GATEWAY_LISTEN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "0.0.0.0:7845".to_string());

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "credential gateway listening");
    axum::serve(listener, router(gateway)).await?;
    Ok(())
}
