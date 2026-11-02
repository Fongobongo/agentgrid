//! TLS termination for the gateway (ADR 0029 step 5, "C3"), no system OpenSSL.
//!
//! This exists for **compatibility, not security**: the agent's leg is inside
//! the sandbox either way, so terminating TLS here hides nothing from the agent.
//! What it buys is a CLI that refuses an `http://` base URL — some do, and the
//! alternative is ADR 0029 option D (an `ssl_bump` proxy with a CA in every
//! image), which is a much larger blast radius for the same result.
//!
//! The sandbox therefore has to trust the operator's CA. The daemon does that
//! side (`AGENTGRID_CRED_GATEWAY_CA` in `sandbox.rs`); this module only serves.
//!
//! Deliberately not shared with `crates/control-plane/src/tls.rs`: that file
//! lives in the crate that carries the database, and `agentgrid-common` is the
//! crate CI runs Miri over — pulling `axum` and `rustls` into it would change
//! what that gate means. Two ~50-line copies are the cheaper trade; if a third
//! consumer appears, the listener moves to its own crate.

use std::sync::Arc;

use anyhow::Context;

pub struct TlsListener {
    pub tcp: tokio::net::TcpListener,
    pub acceptor: tokio_rustls::TlsAcceptor,
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

/// Build a rustls acceptor from a PEM cert chain + private key.
pub fn load_tls_acceptor(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    // rustls-pemfile is archived (RUSTSEC-2025-0134); the same PEM parsing
    // ships in rustls-pki-types as PemObject.
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

/// What the listener should do, decided from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Plain,
    Tls { cert: String, key: String },
}

/// `Ok(Plain)` when neither variable is set, `Ok(Tls)` when both are, and an
/// error when only one is.
///
/// The half-configured case is an error on purpose. A gateway that quietly
/// serves plaintext because the operator typo'd one variable name is the
/// "silently unenforced" failure this project keeps finding: the sandbox still
/// works, so nobody notices until something assumes the traffic was encrypted.
pub fn mode_from_env() -> anyhow::Result<Mode> {
    let cert = nonempty("AGENTGRID_CRED_GATEWAY_TLS_CERT");
    let key = nonempty("AGENTGRID_CRED_GATEWAY_TLS_KEY");
    match (cert, key) {
        (None, None) => Ok(Mode::Plain),
        (Some(cert), Some(key)) => Ok(Mode::Tls { cert, key }),
        (Some(_), None) => anyhow::bail!(
            "AGENTGRID_CRED_GATEWAY_TLS_CERT is set but AGENTGRID_CRED_GATEWAY_TLS_KEY is not: \
             refusing to serve plaintext when TLS was half-configured"
        ),
        (None, Some(_)) => anyhow::bail!(
            "AGENTGRID_CRED_GATEWAY_TLS_KEY is set but AGENTGRID_CRED_GATEWAY_TLS_CERT is not: \
             refusing to serve plaintext when TLS was half-configured"
        ),
    }
}

fn nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_variables_means_plaintext() {
        // The test process never sets these; `mode_from_env` is the only reader
        // and no other test writes them.
        assert_eq!(mode_from_env().unwrap(), Mode::Plain);
    }

    #[test]
    fn a_missing_or_garbage_pem_is_an_error() {
        assert!(load_tls_acceptor("/no/such/cert.pem", "/no/such/key.pem").is_err());
    }
}
