//! `agentgrid-cred-gateway` — see `lib.rs` and ADR 0029.
//!
//! Configuration:
//!
//!   AGENTGRID_CRED_GATEWAY_ROUTES="anthropic|x-api-key|https://api.anthropic.com"
//!   AGENTGRID_CRED_GATEWAY_KEYS="anthropic=sk-ant-…"
//!   AGENTGRID_CRED_GATEWAY_ADMIN=<shared secret, same value the node daemon has>
//!   AGENTGRID_CRED_GATEWAY_LISTEN=0.0.0.0:7845   (default)
//!
//! Optional:
//!
//!   AGENTGRID_CRED_GATEWAY_EXCHANGE="github=github-app;app_id=1;installation_id=2;key=/etc/ag/app.pem"
//!   AGENTGRID_CRED_GATEWAY_TLS_CERT=/etc/ag/gw.pem
//!   AGENTGRID_CRED_GATEWAY_TLS_KEY=/etc/ag/gw-key.pem

use std::sync::Arc;

use agentgrid_cred_gateway::tls::{self, Mode, TlsListener};
use agentgrid_cred_gateway::{router, Gateway};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Before the subscriber and before `from_env`: the release smoke test runs
    // every published binary with `--version`, and a binary that needs a full
    // config to answer it would fail the release rather than the check.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("agentgrid-cred-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

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

    // Decided before binding, so a half-configured TLS refuses to start rather
    // than coming up plaintext and looking healthy.
    let mode = tls::mode_from_env()?;

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    match mode {
        Mode::Plain => {
            tracing::info!(%listen, "credential gateway listening");
            axum::serve(listener, router(gateway)).await?;
        }
        Mode::Tls { cert, key } => {
            // rustls needs a crypto provider installed before any config is
            // built; `ring` is already the backend the control plane uses.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let acceptor = tls::load_tls_acceptor(&cert, &key)?;
            tracing::info!(%listen, "credential gateway listening with TLS");
            axum::serve(
                TlsListener {
                    tcp: listener,
                    acceptor,
                },
                router(gateway),
            )
            .await?;
        }
    }
    Ok(())
}
