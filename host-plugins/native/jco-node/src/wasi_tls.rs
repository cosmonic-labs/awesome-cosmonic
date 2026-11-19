//! A `wasi:tls` client provider that behaves as Jco's Node provider (`tls-host-node`), for hosts
//! whose components reach TLS through `wasi:tls` or `jco:node/tls.start-tls` (`node:https` built
//! over `wasi:sockets`).
//!
//! Like Jco's, it offers ALPN `http/1.1`, sends SNI only for a name that isn't an IP address,
//! verifies the server against the CAs it is given (or the public web roots), gives up on a
//! handshake after a timeout, and reports failures with Node's messages.
//!
//! The provider is the host's to install, as with any `wasi:tls` backend:
//!
//! ```ignore
//! let provider = NodeTlsProvider::new(&[ca_pem], Duration::from_secs(10))?;
//! Engine::builder().with_tls_provider(SharedTlsProvider::new(provider))
//! ```

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use rustls::{ClientConfig, pki_types::ServerName};
use tokio_rustls::TlsConnector;
use wasmtime_wasi_tls::{Error, TlsProvider, TlsStream, TlsTransport};

use crate::hosts::tls_config::{crypto, describe, trust};

/// See the [module documentation](self).
#[derive(Clone)]
pub struct NodeTlsProvider {
    config: Arc<ClientConfig>,
    handshake_timeout: Duration,
}

impl NodeTlsProvider {
    /// A provider trusting `ca` (PEM or DER certificates), or the public web roots when it is
    /// empty, and failing handshakes that take longer than `handshake_timeout`.
    pub fn new(ca: &[Vec<u8>], handshake_timeout: Duration) -> anyhow::Result<Self> {
        let provider = crypto();
        let verifier = trust(ca, provider.clone()).map_err(|failure| anyhow::anyhow!("{}: {}", failure.code, failure.message))?;
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self { config: Arc::new(config), handshake_timeout })
    }
}

impl Default for NodeTlsProvider {
    /// The public web roots, and Jco's ten-second handshake timeout.
    fn default() -> Self {
        Self::new(&[], Duration::from_secs(10)).expect("the web roots make a valid verifier")
    }
}

type Connecting = Pin<Box<dyn Future<Output = Result<Box<dyn TlsStream>, Error>> + Send>>;

impl TlsProvider for NodeTlsProvider {
    fn connect(&self, server_name: String, transport: Box<dyn TlsTransport>) -> Connecting {
        let connector = TlsConnector::from(self.config.clone());
        let timeout = self.handshake_timeout;
        Box::pin(async move {
            let name = ServerName::try_from(server_name.clone()).map_err(|_| Error::msg(format!("invalid server name {server_name}")))?;
            match tokio::time::timeout(timeout, connector.connect(name, transport)).await {
                Err(_) => Err(Error::msg("TLS handshake timed out")),
                Ok(Err(failure)) => Err(Error::msg(match failure.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
                    Some(tls) => {
                        let (code, message) = describe(tls, &server_name);
                        format!("{code}: {message}")
                    }
                    None => failure.to_string(),
                })),
                Ok(Ok(stream)) => Ok(Box::new(stream) as Box<dyn TlsStream>),
            }
        })
    }
}
