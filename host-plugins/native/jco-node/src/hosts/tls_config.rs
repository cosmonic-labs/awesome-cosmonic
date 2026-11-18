//! rustls setups built from a `jco:node/tls` secure context, shared by the hosts that terminate
//! or originate TLS themselves (`http` for `node:https`, `http2`).

use std::sync::Arc;

use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_name,
    },
    server::ParsedCertificate,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::hosts::tls::SecureContext;

/// Why a TLS setup could not be built, as a Node error code and message.
#[derive(Debug)]
pub(crate) struct SetupError {
    pub code: &'static str,
    pub message: String,
}

fn failure(error: impl std::fmt::Display) -> SetupError {
    SetupError { code: "ERR_SSL_INVALID_CONTEXT", message: error.to_string() }
}

/// Certificates given as PEM, or as one DER certificate.
fn certificates(material: &[u8]) -> Vec<CertificateDer<'static>> {
    let parsed: Vec<_> = CertificateDer::pem_slice_iter(material).filter_map(Result::ok).collect();
    if parsed.is_empty() { vec![CertificateDer::from(material.to_vec())] } else { parsed }
}

fn private_key(material: &[u8]) -> Option<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(material).ok().or_else(|| PrivateKeyDer::try_from(material.to_vec()).ok())
}

fn crypto() -> Arc<CryptoProvider> {
    CryptoProvider::get_default().cloned().unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// A server's TLS setup from its secure context, offering `alpn`.
pub(crate) fn server(context: &SecureContext, alpn: &[&[u8]]) -> Result<TlsAcceptor, SetupError> {
    let certs = context.cert.as_deref().map(certificates).unwrap_or_default();
    let key = context.key.as_deref().and_then(private_key).ok_or_else(|| SetupError {
        code: "ERR_SSL_NO_PRIVATE_KEY",
        message: "A TLS server needs a private key".into(),
    })?;
    let mut config = ServerConfig::builder_with_provider(crypto())
        .with_safe_default_protocol_versions()
        .map_err(failure)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(failure)?;
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Accepts any server certificate, for `rejectUnauthorized: false`; signatures are still checked.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, signature, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, signature, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Verification as Node's OpenSSL does it for a context's `ca`: a server presenting one of those
/// certificates itself (a self-signed certificate given as its own CA) is trusted, its name still
/// checked; anything else goes through webpki against the CAs as roots.
#[derive(Debug)]
struct TrustedOrWebPki {
    pinned: Vec<CertificateDer<'static>>,
    webpki: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for TrustedOrWebPki {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.pinned.iter().any(|pinned| pinned.as_ref() == end_entity.as_ref()) {
            let parsed = ParsedCertificate::try_from(end_entity)?;
            verify_server_name(&parsed, server_name)?;
            return Ok(ServerCertVerified::assertion());
        }
        self.webpki.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// A client's TLS setup: the context's CAs (or the public web roots), unless it turns
/// verification off; offering `alpn`.
pub(crate) fn client(context: Option<&SecureContext>, alpn: &[&[u8]]) -> Result<TlsConnector, SetupError> {
    let provider = crypto();
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().map_err(failure)?;
    let mut config = if context.and_then(|context| context.reject_unauthorized) == Some(false) {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider))).with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        let authorities: Vec<_> =
            context.map(|context| context.ca.iter().flat_map(|ca| certificates(ca)).collect()).unwrap_or_default();
        for authority in &authorities {
            let _ = roots.add(authority.clone());
        }
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().map_err(failure)?;
        let verifier = TrustedOrWebPki { pinned: authorities, webpki };
        builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth()
    };
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    Ok(TlsConnector::from(Arc::new(config)))
}
