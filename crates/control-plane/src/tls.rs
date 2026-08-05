//! TLS listener + graceful shutdown (no system OpenSSL; rustls only).

use std::sync::Arc;

use anyhow::Context;

use crate::AppState;

pub struct TlsListener {
    pub(crate) tcp: tokio::net::TcpListener,
    pub(crate) acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.tcp.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls) => return (tls, addr),
                    Err(e) => tracing::warn!("tls handshake failed: {e}"),
                },
                Err(e) => {
                    tracing::error!("accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// Build a rustls acceptor from a PEM cert chain + private key (no system OpenSSL).
pub fn load_tls_acceptor(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    let cert_pem =
        std::fs::read(cert_path).with_context(|| format!("read TLS cert {cert_path}"))?;
    let key_pem = std::fs::read(key_path).with_context(|| format!("read TLS key {key_path}"))?;
    let mut cert_reader = std::io::Cursor::new(&cert_pem[..]);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<_, _>>()?;
    let mut key_reader = std::io::Cursor::new(&key_pem[..]);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .context("no private key found in TLS key PEM")?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build rustls server config")?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Await Ctrl-C / SIGTERM, then truncate the WAL so a restart replays nothing
/// stale (Stage 2.5 ops).
pub async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                let _ = sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    let _ = state.store.wal_checkpoint().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_tls_acceptor_missing_file_errors() {
        assert!(load_tls_acceptor("/no/such/cert.pem", "/no/such/key.pem").is_err());
    }
}
