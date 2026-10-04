//! Connecting to a mail server over TLS.
//!
//! IMAP uses implicit TLS only (port 993 style): no STARTTLS, whose
//! plaintext prelude can be stripped, and never plaintext. SMTP uses
//! implicit TLS (port 465) or, when the owner says so, STARTTLS (port 587),
//! upgraded with [`upgrade`] before anything but the greeting and `EHLO` is
//! sent; a server that refuses the upgrade is never used. Certificates are
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

/// A TLS connection to the IMAP server `host:port`, the certificate
/// verified for `host`.
pub(crate) async fn connect(host: &str, port: u16) -> Result<TlsStream<TcpStream>> {
    connect_to(host, port, "IMAP").await
}

/// A TLS connection to the `what` (`IMAP`, `SMTP`) server `host:port`.
pub(crate) async fn connect_to(
    host: &str,
    port: u16,
    what: &'static str,
) -> Result<TlsStream<TcpStream>> {
    server_name(host, what)?;
    let tcp = tcp(host, port, what).await?;
    upgrade(host, tcp, what).await
}

/// `host` as the name its certificate must carry.
fn server_name(host: &str, what: &'static str) -> Result<ServerName<'static>> {
    ServerName::try_from(host.to_owned())
        .with_context(|| format!("the {what} host is not a valid server name"))
}

/// A plain TCP connection to `host:port`, for a protocol that upgrades it.
pub(crate) async fn tcp(host: &str, port: u16, what: &'static str) -> Result<TcpStream> {
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .with_context(|| format!("connecting to the {what} server timed out"))?
        .with_context(|| format!("could not connect to the {what} server"))?;
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

/// TLS over `tcp`, the certificate verified for `host`.
pub(crate) async fn upgrade(
    host: &str,
    tcp: TcpStream,
    what: &'static str,
) -> Result<TlsStream<TcpStream>> {
    let name = server_name(host, what)?;
    let connector = TlsConnector::from(Arc::new(client_config()?));
    timeout(HANDSHAKE_TIMEOUT, connector.connect(name, tcp))
        .await
        .with_context(|| format!("the TLS handshake with the {what} server timed out"))?
        .with_context(|| format!("the TLS handshake with the {what} server failed"))
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
