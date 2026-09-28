//! Connecting to an IMAP server over implicit TLS.
//!
//! Only implicit TLS (port 993 style) is offered: no STARTTLS, whose
//! plaintext prelude can be stripped, and never plaintext. Certificates are
//! checked against the Mozilla roots bundled in `webpki-roots`, so the
//! host's trust store cannot widen what is accepted.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Opening the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The TLS handshake once connected.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// A TLS connection to `host:port`, the certificate verified for `host`.
pub(crate) async fn connect(host: &str, port: u16) -> Result<TlsStream<TcpStream>> {
    let name = ServerName::try_from(host.to_owned())
        .context("the IMAP host is not a valid server name")?;
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .context("connecting to the IMAP server timed out")?
        .context("could not connect to the IMAP server")?;
    tcp.set_nodelay(true)?;
    let connector = TlsConnector::from(Arc::new(client_config()?));
    timeout(HANDSHAKE_TIMEOUT, connector.connect(name, tcp))
        .await
        .context("the TLS handshake with the IMAP server timed out")?
        .context("the TLS handshake with the IMAP server failed")
}

fn client_config() -> Result<rustls::ClientConfig> {
    let roots: rustls::RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

#[cfg(test)]
mod tests;
