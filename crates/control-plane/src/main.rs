use std::net::SocketAddr;

use agentgrid_control_plane::{serve, AppState};
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let addr: SocketAddr = std::env::var("AGENTGRID_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:7800".into())
        .parse()?;

    serve(AppState::new(), addr).await
}
