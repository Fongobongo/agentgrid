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
    // rustls-pemfile is archived (RUSTSEC-2025-0134); the same PEM parsing
    // ships in rustls-pki-types ≥ 1.11 as PemObject.
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_file_iter(cert_path)
            .with_context(|| format!("parse TLS cert chain {cert_path}"))?
            .collect::<Result<_, _>>()?;
    let key = rustls::pki_types::PrivateKeyDer::pem_file_iter(key_path)
        .with_context(|| format!("parse TLS key {key_path}"))?
        .next()
        .transpose()?
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

    #[test]
    fn load_tls_acceptor_rejects_non_pem_garbage() {
        // Exercises the pki-types PemObject parser path (RUSTSEC-2025-0134
        // replacement): a file that is not PEM-shaped must error, not panic
        // or unwrap. Real DER certs are not hand-rolled here (no test-only
        // crypto dep by design) — the happy path is covered by the e2e TLS
        // smoke tests against real generated certs.
        let dir = std::env::temp_dir();
        let cert = dir.join("ag-tls-test-garbage-cert.pem");
        let key = dir.join("ag-tls-test-garbage-key.pem");
        std::fs::write(&cert, b"this is not a PEM file").unwrap();
        std::fs::write(&key, b"neither is this").unwrap();
        assert!(load_tls_acceptor(cert.to_str().unwrap(), key.to_str().unwrap()).is_err());
        // Same file existing but only a cert PEM (no key section) must also
        // error on the key side.
        std::fs::write(
            &key,
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        assert!(load_tls_acceptor(cert.to_str().unwrap(), key.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }
}
