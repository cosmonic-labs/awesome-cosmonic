//! Node TLS sockets and servers (`tls.connect`, `tls.createServer`) on rustls.
//!
//! Each socket is a task that owns its connection. The guest drives it with [`Command`]s and
//! hears back through `jco:node/tls-callbacks` events, queued as [`Job::Tls`] in the order they
//! happen. What the guest reads synchronously (addresses, certificates, the negotiated cipher) is
//! kept in a [`SocketInfo`] the task updates.
//!
//! As in Jco's Node provider, a socket delivers one `data` event and then waits for the guest to
//! `resume` it, so the guest's stream backpressure reaches the connection.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as Tls, HandshakeKind, ProtocolVersion,
    RootCertStore, ServerConfig, SignatureScheme, SupportedProtocolVersion,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::{
        ClientHello, ResolvesServerCert, WebPkiClientVerifier,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
    sign::CertifiedKey,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::AbortHandle,
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use super::{
    SecureContext,
    certificate::name_matches,
    wire::{Value, error, error_with, object},
};
use crate::{
    bindings::jco::node::http::{Errno, Error as HttpError},
    callbacks::{Job, TlsEvent},
    hosts::{
        http,
        tls_config::{SetupError, TrustedOrWebPki, certificates, crypto, describe, private_key, trust},
    },
    state::lock,
};

// ---- Events -----------------------------------------------------------------------------------

/// Queues `jco:node/tls-callbacks` events for one socket or server.
#[derive(Clone)]
pub(crate) struct Events {
    target: u32,
    jobs: mpsc::UnboundedSender<Job>,
}

impl Events {
    pub(crate) fn new(target: u32, jobs: mpsc::UnboundedSender<Job>) -> Self {
        Self { target, jobs }
    }

    pub(crate) fn send(&self, name: &str, value: Value) {
        self.send_data(name, value, Vec::new());
    }

    fn send_data(&self, name: &str, value: Value, data: Vec<u8>) {
        let _ = self.jobs.send(Job::Tls(TlsEvent { target: self.target, name: name.into(), value: value.encode(), data }));
    }
}

/// A `node:http` error record as a wire error.
pub(crate) fn http_error(failure: HttpError) -> Value {
    let errno = match failure.errno {
        Some(Errno::Number(number)) => Value::Number(number as f64),
        Some(Errno::Symbolic(symbol)) => Value::Text(symbol),
        None => Value::Undefined,
    };
    let mut value = error_with(
        failure.code.as_deref(),
        failure.message,
        [
            ("errno", errno),
            ("syscall", failure.syscall.into()),
            ("hostname", failure.hostname.into()),
            ("address", failure.address.into()),
            ("port", failure.port.into()),
        ],
    );
    if let Value::Error(name, _) = &mut value {
        *name = failure.name;
    }
    value
}

/// An I/O failure on a TLS connection: the verification error Node reports for a rustls failure,
/// else the socket error.
fn io_error(failure: &io::Error, syscall: &str, servername: &str) -> Value {
    match failure.get_ref().and_then(|inner| inner.downcast_ref::<Tls>()) {
        Some(tls) => {
            let (code, message) = describe(tls, servername);
            error(Some(code), message)
        }
        None => http_error(http::socket_error(failure, syscall, None)),
    }
}

// ---- Shared state -----------------------------------------------------------------------------

/// What the guest reads from a socket without waiting on it.
#[derive(Default)]
pub(crate) struct SocketInfo {
    pub(crate) connecting: bool,
    pub(crate) server: bool,
    pub(crate) authorized: bool,
    pub(crate) authorization_error: Option<String>,
    pub(crate) servername: Option<String>,
    pub(crate) alpn: Option<String>,
    pub(crate) local: Option<SocketAddr>,
    pub(crate) remote: Option<SocketAddr>,
    pub(crate) bytes_read: u64,
    pub(crate) bytes_written: u64,
    pub(crate) peer_chain: Vec<CertificateDer<'static>>,
    pub(crate) own_certificate: Option<CertificateDer<'static>>,
    pub(crate) protocol: Option<&'static str>,
    pub(crate) suite: Option<rustls::CipherSuite>,
    pub(crate) key_exchange: Option<rustls::NamedGroup>,
    pub(crate) resumed: bool,
}

fn family(address: &SocketAddr) -> &'static str {
    if address.is_ipv4() { "IPv4" } else { "IPv6" }
}

pub(crate) fn address(address: Option<SocketAddr>) -> Value {
    match address {
        Some(address) => object([
            ("address", address.ip().to_string().into()),
            ("family", family(&address).into()),
            ("port", address.port().into()),
        ]),
        None => Value::Object(Vec::new()),
    }
}

impl SocketInfo {
    /// The `SocketState` the guest's `TLSSocket` mirrors.
    pub(crate) fn state(&self) -> Value {
        let text_or_false = |value: &Option<String>| value.clone().map_or(Value::Bool(false), Value::Text);
        object([
            ("authorized", self.authorized.into()),
            ("authorizationError", self.authorization_error.clone().into()),
            ("alpnProtocol", text_or_false(&self.alpn)),
            ("servername", text_or_false(&self.servername)),
            ("localAddress", self.local.map(|local| local.ip().to_string()).into()),
            ("localPort", self.local.map(|local| local.port()).into()),
            ("localFamily", self.local.as_ref().map(family).into()),
            ("remoteAddress", self.remote.map(|remote| remote.ip().to_string()).into()),
            ("remotePort", self.remote.map(|remote| remote.port()).into()),
            ("remoteFamily", self.remote.as_ref().map(family).into()),
            ("bytesRead", self.bytes_read.into()),
            ("bytesWritten", self.bytes_written.into()),
            ("connecting", self.connecting.into()),
            ("pending", self.connecting.into()),
        ])
    }

    /// `getCipher()`: OpenSSL's name, the IANA name, and the protocol version.
    pub(crate) fn cipher(&self) -> Value {
        let Some(standard) = self.suite.and_then(|suite| suite.as_str()) else { return Value::Object(Vec::new()) };
        let standard = match standard.strip_prefix("TLS13_") {
            Some(rest) => format!("TLS_{rest}"),
            None => standard.to_owned(),
        };
        let name = match standard.strip_prefix("TLS_").and_then(|rest| rest.split_once("_WITH_")) {
            // TLS 1.2: ECDHE_RSA + AES_128_GCM_SHA256 is OpenSSL's ECDHE-RSA-AES128-GCM-SHA256.
            Some((exchange, cipher)) => {
                let cipher = cipher.replace("AES_128", "AES128").replace("AES_256", "AES256");
                let cipher = cipher.strip_suffix("_SHA256").filter(|cipher| cipher.contains("CHACHA20")).unwrap_or(&cipher);
                format!("{}-{}", exchange, cipher).replace('_', "-")
            }
            None => standard.clone(),
        };
        object([
            ("name", name.into()),
            ("standardName", standard.into()),
            ("version", self.protocol.map(str::to_owned).into()),
        ])
    }

    /// `getEphemeralKeyInfo()`: the key exchange a client negotiated (`{}` on a server).
    pub(crate) fn ephemeral_key(&self) -> Value {
        if self.server {
            return Value::Object(Vec::new());
        }
        let Some(group) = self.key_exchange else { return Value::Null };
        let (name, size) = match group {
            rustls::NamedGroup::X25519 => ("X25519".to_owned(), Value::from(253u32)),
            rustls::NamedGroup::secp256r1 => ("prime256v1".to_owned(), Value::from(256u32)),
            rustls::NamedGroup::secp384r1 => ("secp384r1".to_owned(), Value::from(384u32)),
            rustls::NamedGroup::secp521r1 => ("secp521r1".to_owned(), Value::from(521u32)),
            other => (other.as_str().unwrap_or("unknown").to_owned(), Value::Undefined),
        };
        object([("type", "ECDH".into()), ("name", name.into()), ("size", size)])
    }

    fn record(&mut self, common: &rustls::CommonState) {
        self.connecting = false;
        self.alpn = common.alpn_protocol().map(|protocol| String::from_utf8_lossy(protocol).into_owned());
        self.peer_chain = common
            .peer_certificates()
            .map(|chain| chain.iter().map(|certificate| certificate.clone().into_owned()).collect())
            .unwrap_or_default();
        self.protocol = common.protocol_version().map(|version| match version {
            ProtocolVersion::TLSv1_3 => "TLSv1.3",
            ProtocolVersion::TLSv1_2 => "TLSv1.2",
            _ => "unknown",
        });
        self.suite = common.negotiated_cipher_suite().map(|suite| suite.suite());
        self.key_exchange = common.negotiated_key_exchange_group().map(|group| group.name());
        self.resumed = common.handshake_kind() == Some(HandshakeKind::Resumed);
    }
}

/// What the guest asks of a connected socket.
pub(crate) enum Command {
    Write { token: u32, data: Vec<u8> },
    End { token: u32 },
    Resume,
    Pause,
    Timeout(u32),
    NoDelay(bool),
    KeepAlive(bool, u32),
    MaxFragment(usize),
    ExportKeyingMaterial { length: usize, label: Vec<u8>, context: Option<Vec<u8>>, reply: oneshot::Sender<Result<Vec<u8>, String>> },
}

pub(crate) struct SocketHandle {
    pub(crate) info: Arc<Mutex<SocketInfo>>,
    pub(crate) commands: mpsc::UnboundedSender<Command>,
    /// Dropped on release, which ends the socket's task.
    _cancel: oneshot::Sender<()>,
}

/// The guest's live sockets and servers, by the ids it allocated.
#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) sockets: HashMap<u32, SocketHandle>,
    pub(crate) servers: HashMap<u32, Arc<ServerShared>>,
    accepted: u32,
}

impl Registry {
    /// Accepted sockets are numbered down from `0x7fffffff`, clear of the guest's own ids.
    fn next_accepted(&mut self) -> u32 {
        self.accepted += 1;
        0x8000_0000 - self.accepted
    }

    fn register(&mut self, id: u32, info: Arc<Mutex<SocketInfo>>) -> (mpsc::UnboundedReceiver<Command>, oneshot::Receiver<()>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        let (cancel, cancelled) = oneshot::channel();
        self.sockets.insert(id, SocketHandle { info, commands, _cancel: cancel });
        (receiver, cancelled)
    }
}

// ---- Trust ------------------------------------------------------------------------------------

/// How a client verifies servers, as Node does: against the context's CAs (or the default roots),
/// the name checked unless the guest checks it itself (`checkServerIdentity`). With
/// `rejectUnauthorized: false` the handshake goes on regardless, and the outcome is only reported.
#[derive(Debug)]
pub(crate) struct ServerTrust {
    trust: TrustedOrWebPki,
    reject: bool,
    check_name: bool,
}

impl ServerTrust {
    fn check(&self, chain: &[CertificateDer<'_>], name: &ServerName<'_>) -> Result<(), Tls> {
        let Some((end_entity, intermediates)) = chain.split_first() else { return Err(Tls::NoCertificatesPresented) };
        match self.trust.verify_server_cert(end_entity, intermediates, name, &[], UnixTime::now()) {
            Err(Tls::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }))
                if !self.check_name =>
            {
                Ok(())
            }
            result => result.map(|_| ()),
        }
    }
}

impl ServerCertVerifier for ServerTrust {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Tls> {
        let mut chain = vec![end_entity.clone()];
        chain.extend(intermediates.iter().cloned());
        match self.check(&chain, server_name) {
            Err(failure) if self.reject => Err(failure),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Tls> {
        self.trust.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Tls> {
        self.trust.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.trust.supported_verify_schemes()
    }
}

/// How a server verifies clients with `requestCert`: a client presenting one of the context's CAs
/// itself is trusted, anything else must chain to them. With `rejectUnauthorized: false` any
/// client is let in and the outcome only reported.
#[derive(Debug)]
pub(crate) struct ClientTrust {
    pinned: Vec<CertificateDer<'static>>,
    webpki: Option<Arc<dyn ClientCertVerifier>>,
    provider: Arc<CryptoProvider>,
    request: bool,
    reject: bool,
    hints: Vec<DistinguishedName>,
}

impl ClientTrust {
    fn new(ca: &[Vec<u8>], request: bool, reject: bool) -> Self {
        let provider = crypto();
        let pinned: Vec<_> = ca.iter().flat_map(|ca| certificates(ca)).collect();
        let mut roots = RootCertStore::empty();
        for authority in &pinned {
            let _ = roots.add(authority.clone());
        }
        let webpki = (!roots.is_empty())
            .then(|| WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().ok())
            .flatten();
        Self { pinned, webpki, provider, request, reject, hints: Vec::new() }
    }

    fn check(&self, chain: &[CertificateDer<'_>]) -> Result<(), Tls> {
        let Some((end_entity, intermediates)) = chain.split_first() else { return Err(Tls::NoCertificatesPresented) };
        if self.pinned.iter().any(|pinned| pinned.as_ref() == end_entity.as_ref()) {
            return Ok(());
        }
        match &self.webpki {
            Some(webpki) => webpki.verify_client_cert(end_entity, intermediates, UnixTime::now()).map(|_| ()),
            None => Err(Tls::InvalidCertificate(CertificateError::UnknownIssuer)),
        }
    }
}

impl ClientCertVerifier for ClientTrust {
    fn offer_client_auth(&self) -> bool {
        self.request
    }

    fn client_auth_mandatory(&self) -> bool {
        self.request && self.reject
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.hints
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, Tls> {
        let mut chain = vec![end_entity.clone()];
        chain.extend(intermediates.iter().cloned());
        match self.check(&chain) {
            Err(failure) if self.reject => Err(failure),
            _ => Ok(ClientCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Tls> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Tls> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The protocol versions `minVersion`/`maxVersion` allow, of the two rustls speaks.
pub(crate) fn versions(options: &Value) -> Result<Vec<&'static SupportedProtocolVersion>, Value> {
    let rank = |key: &str, default: u8| -> Result<u8, Value> {
        match options.get(key).and_then(Value::text) {
            None => Ok(default),
            Some("TLSv1") => Ok(10),
            Some("TLSv1.1") => Ok(11),
            Some("TLSv1.2") => Ok(12),
            Some("TLSv1.3") => Ok(13),
            Some(other) => Err(error(Some("ERR_TLS_INVALID_PROTOCOL_VERSION"), format!("{other} is not a valid TLS protocol version"))),
        }
    };
    let (min, max) = (rank("minVersion", 12)?, rank("maxVersion", 13)?);
    let mut allowed = Vec::new();
    if min <= 12 && max >= 12 {
        allowed.push(&rustls::version::TLS12);
    }
    if max >= 13 {
        allowed.push(&rustls::version::TLS13);
    }
    if allowed.is_empty() {
        return Err(error(Some("ERR_TLS_INVALID_PROTOCOL_VERSION"), "no TLS protocol version this host supports is allowed"));
    }
    Ok(allowed)
}

/// `ALPNProtocols`: names as strings or bytes, or one buffer in wire format (length-prefixed).
pub(crate) fn alpn(value: Option<&Value>) -> Vec<Vec<u8>> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(Value::material).collect(),
        Some(Value::Bytes(wire)) => {
            let mut protocols = Vec::new();
            let mut rest = wire.as_slice();
            while let Some((&length, tail)) = rest.split_first() {
                let length = usize::from(length).min(tail.len());
                protocols.push(tail[..length].to_vec());
                rest = &tail[length..];
            }
            protocols
        }
        _ => Vec::new(),
    }
}

fn setup_error(failure: SetupError) -> Value {
    error(Some(failure.code), failure.message)
}

fn certified(context: &SecureContext, provider: &CryptoProvider) -> Result<Option<Arc<CertifiedKey>>, Value> {
    let (Some(cert), Some(key)) = (&context.cert, &context.key) else { return Ok(None) };
    let key = private_key(key).ok_or_else(|| error(Some("ERR_OSSL_UNSUPPORTED"), "the private key could not be decoded"))?;
    CertifiedKey::from_der(certificates(cert), key, provider)
        .map(|key| Some(Arc::new(key)))
        .map_err(|failure| error(Some("ERR_OSSL_X509_KEY_VALUES_MISMATCH"), failure.to_string()))
}

// ---- Clients ----------------------------------------------------------------------------------

/// A `tls.connect` ready to run.
pub(crate) struct Connect {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: ServerName<'static>,
    pub(crate) connector: TlsConnector,
    pub(crate) trust: Arc<ServerTrust>,
    pub(crate) timeout: Option<u32>,
}

/// The client setup for `options` over `context`.
pub(crate) fn client(
    context: &SecureContext,
    options: &Value,
    host: &str,
) -> Result<(TlsConnector, Arc<ServerTrust>, ServerName<'static>), Value> {
    let provider = crypto();
    let servername = options.get("servername").and_then(Value::text).filter(|name| !name.is_empty());
    let name = servername.or(context.servername.as_deref()).unwrap_or(host);
    let name = ServerName::try_from(name.to_owned())
        .map_err(|_| error(Some("ERR_INVALID_ARG_VALUE"), format!("{name} is not a valid server name")))?;
    let reject = options.get("rejectUnauthorized").and_then(Value::boolean).or(context.reject_unauthorized).unwrap_or(true);
    let trust = Arc::new(ServerTrust {
        trust: trust(&context.ca, provider.clone()).map_err(setup_error)?,
        reject,
        check_name: options.get("customIdentity").and_then(Value::boolean) != Some(true),
    });
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&versions(options)?)
        .map_err(|failure| error(Some("ERR_SSL_INVALID_CONTEXT"), failure.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(trust.clone());
    let mut config = match (&context.cert, &context.key) {
        (Some(cert), Some(key)) => {
            let key = private_key(key).ok_or_else(|| error(Some("ERR_OSSL_UNSUPPORTED"), "the private key could not be decoded"))?;
            builder
                .with_client_auth_cert(certificates(cert), key)
                .map_err(|failure| error(Some("ERR_OSSL_X509_KEY_VALUES_MISMATCH"), failure.to_string()))?
        }
        _ => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpn(options.get("ALPNProtocols"));
    Ok((TlsConnector::from(Arc::new(config)), trust, name))
}

/// Connect, handshake, then serve the socket until it closes or is released.
pub(crate) async fn run_client(
    plan: Connect,
    info: Arc<Mutex<SocketInfo>>,
    commands: mpsc::UnboundedReceiver<Command>,
    mut cancelled: oneshot::Receiver<()>,
    events: Events,
) {
    let name = match &plan.name {
        ServerName::DnsName(name) => name.as_ref().to_owned(),
        ServerName::IpAddress(address) => std::net::IpAddr::from(*address).to_string(),
        _ => plan.host.clone(),
    };
    let fail = |failure: Value| {
        events.send("error", failure);
        events.send("close", Value::Bool(true));
    };
    let connecting = async {
        let addresses = http::resolve(&plan.host, plan.port).await.map_err(http_error)?;
        let tcp = http::connect(&addresses, plan.timeout).await.map_err(http_error)?;
        {
            let mut info = lock(&info);
            info.local = tcp.local_addr().ok();
            info.remote = tcp.peer_addr().ok();
        }
        events.send("connect", Value::Undefined);
        plan.connector.connect(plan.name.clone(), tcp).await.map_err(|failure| io_error(&failure, "connect", &name))
    };
    let stream = tokio::select! {
        stream = connecting => match stream {
            Ok(stream) => stream,
            Err(failure) => return fail(failure),
        },
        _ = &mut cancelled => return,
    };
    let state = {
        let mut info = lock(&info);
        let connection = stream.get_ref().1;
        info.record(connection);
        if let ServerName::DnsName(dns) = &plan.name {
            info.servername = Some(dns.as_ref().to_owned());
        }
        let verdict = plan.trust.check(&info.peer_chain, &plan.name);
        info.authorized = verdict.is_ok();
        info.authorization_error = verdict.err().map(|failure| describe(&failure, &name).0.to_owned());
        info.state()
    };
    events.send("secureConnect", state);
    serve(TlsStream::Client(stream), info, commands, cancelled, events, name).await;
}

// ---- Connected sockets ------------------------------------------------------------------------

async fn write(stream: &mut TlsStream<TcpStream>, data: &[u8], fragment: Option<usize>) -> io::Result<()> {
    // rustls seals each write into records of at most its size, so a `setMaxSendFragment` limit
    // holds by writing in pieces no larger.
    for piece in data.chunks(fragment.unwrap_or(data.len()).max(1)) {
        stream.write_all(piece).await?;
    }
    stream.flush().await
}

fn token(token: u32, failure: Option<Value>) -> Value {
    object([("token", token.into()), ("error", failure.unwrap_or(Value::Undefined))])
}

async fn serve(
    mut stream: TlsStream<TcpStream>,
    info: Arc<Mutex<SocketInfo>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut cancelled: oneshot::Receiver<()>,
    events: Events,
    servername: String,
) {
    let mut buffer = vec![0u8; 16 * 1024];
    let (mut reading, mut read_done, mut write_done) = (true, false, false);
    let mut idle: Option<Duration> = None;
    let mut deadline = tokio::time::Instant::now();
    let mut fragment = None;
    let fail = |failure: Value| {
        events.send("error", failure);
        events.send("close", Value::Bool(true));
    };
    loop {
        if read_done && write_done {
            events.send("close", Value::Bool(false));
            return;
        }
        let timer = async {
            match idle {
                Some(_) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut cancelled => return,
            command = commands.recv() => {
                let Some(command) = command else { return };
                match command {
                    Command::Write { token: id, data } => match write(&mut stream, &data, fragment).await {
                        Ok(()) => {
                            lock(&info).bytes_written += data.len() as u64;
                            events.send("write", token(id, None));
                            if let Some(idle) = idle {
                                deadline = tokio::time::Instant::now() + idle;
                            }
                        }
                        Err(failure) => {
                            let failure = io_error(&failure, "write", &servername);
                            events.send("write", token(id, Some(failure.clone())));
                            return fail(failure);
                        }
                    },
                    Command::End { token: id } => {
                        let result = stream.shutdown().await;
                        write_done = true;
                        events.send("write", token(id, result.err().map(|failure| io_error(&failure, "shutdown", &servername))));
                    }
                    Command::Resume => reading = true,
                    Command::Pause => reading = false,
                    Command::Timeout(milliseconds) => {
                        idle = (milliseconds > 0).then(|| Duration::from_millis(u64::from(milliseconds)));
                        deadline = tokio::time::Instant::now() + idle.unwrap_or_default();
                    }
                    Command::NoDelay(enable) => {
                        let _ = stream.get_ref().0.set_nodelay(enable);
                    }
                    Command::KeepAlive(enable, delay) => {
                        let socket = socket2::SockRef::from(stream.get_ref().0);
                        let _ = if enable && delay > 0 {
                            socket.set_tcp_keepalive(&socket2::TcpKeepalive::new().with_time(Duration::from_millis(u64::from(delay))))
                        } else {
                            socket.set_keepalive(enable)
                        };
                    }
                    Command::MaxFragment(size) => fragment = Some(size),
                    Command::ExportKeyingMaterial { length, label, context, reply } => {
                        let output = vec![0u8; length];
                        let result = match &stream {
                            TlsStream::Client(client) => client.get_ref().1.export_keying_material(output, &label, context.as_deref()),
                            TlsStream::Server(server) => server.get_ref().1.export_keying_material(output, &label, context.as_deref()),
                        };
                        let _ = reply.send(result.map_err(|failure| failure.to_string()));
                    }
                }
            }
            read = stream.read(&mut buffer), if reading && !read_done => match read {
                // A peer that closes without a close_notify has still ended, as Node treats it.
                Ok(0) => {
                    read_done = true;
                    events.send("end", Value::Undefined);
                }
                Err(failure) if failure.kind() == io::ErrorKind::UnexpectedEof => {
                    read_done = true;
                    events.send("end", Value::Undefined);
                }
                Ok(read) => {
                    reading = false;
                    lock(&info).bytes_read += read as u64;
                    events.send_data("data", Value::Undefined, buffer[..read].to_vec());
                    if let Some(idle) = idle {
                        deadline = tokio::time::Instant::now() + idle;
                    }
                }
                Err(failure) => return fail(io_error(&failure, "read", &servername)),
            },
            () = timer => {
                events.send("timeout", Value::Undefined);
                deadline = tokio::time::Instant::now() + idle.unwrap_or_default();
            }
        }
    }
}

// ---- Servers ----------------------------------------------------------------------------------

/// A `tls.Server`'s certificates and verification, rebuilt when the guest changes them.
pub(crate) struct ServerSetup {
    pub(crate) context: SecureContext,
    pub(crate) named: Vec<(String, SecureContext)>,
    pub(crate) alpn: Vec<Vec<u8>>,
    pub(crate) request_cert: bool,
    pub(crate) reject: bool,
    pub(crate) versions: Vec<&'static SupportedProtocolVersion>,
    pub(crate) ticket_keys: Vec<u8>,
    pub(crate) handshake_timeout: Duration,
    built: Option<(TlsAcceptor, Arc<ClientTrust>)>,
}

/// Picks the certificate for the name a client asks for (`addContext`), else the server's own.
#[derive(Debug)]
struct ByServerName {
    default: Option<Arc<CertifiedKey>>,
    named: Vec<(String, Arc<CertifiedKey>)>,
}

impl ResolvesServerCert for ByServerName {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = hello.server_name() {
            let name = name.to_ascii_lowercase();
            let labels: Vec<&str> = name.split('.').collect();
            // Node consults the most recently added context first.
            if let Some((_, key)) = self.named.iter().rev().find(|(pattern, _)| name_matches(&labels, pattern)) {
                return Some(key.clone());
            }
        }
        self.default.clone()
    }
}

impl ServerSetup {
    pub(crate) fn new(context: SecureContext, options: &Value) -> Result<Self, Value> {
        let mut ticket_keys = vec![0u8; 48];
        crypto().secure_random.fill(&mut ticket_keys).map_err(|_| error(None, "no secure random source"))?;
        let setup = Self {
            context,
            named: Vec::new(),
            alpn: alpn(options.get("ALPNProtocols")),
            request_cert: options.get("requestCert").and_then(Value::boolean).unwrap_or(false),
            reject: options.get("rejectUnauthorized").and_then(Value::boolean).unwrap_or(true),
            versions: versions(options)?,
            ticket_keys,
            handshake_timeout: Duration::from_millis(
                options.get("handshakeTimeout").and_then(Value::number).filter(|timeout| *timeout > 0.0).unwrap_or(120_000.0) as u64,
            ),
            built: None,
        };
        Ok(setup)
    }

    /// Drop the built setup, after a change.
    pub(crate) fn changed(&mut self) {
        self.built = None;
    }

    fn acceptor(&mut self) -> Result<(TlsAcceptor, Arc<ClientTrust>), Value> {
        if let Some(built) = &self.built {
            return Ok(built.clone());
        }
        let provider = crypto();
        let resolver = ByServerName {
            default: certified(&self.context, &provider)?,
            named: self
                .named
                .iter()
                .map(|(name, context)| Ok(certified(context, &provider)?.map(|key| (name.to_ascii_lowercase(), key))))
                .collect::<Result<Vec<_>, Value>>()?
                .into_iter()
                .flatten()
                .collect(),
        };
        if resolver.default.is_none() && resolver.named.is_empty() {
            return Err(error(Some("ERR_SSL_NO_PRIVATE_KEY"), "A TLS server needs a certificate and private key"));
        }
        let trust = Arc::new(ClientTrust::new(&self.context.ca, self.request_cert, self.reject));
        let builder = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&self.versions)
            .map_err(|failure| error(Some("ERR_SSL_INVALID_CONTEXT"), failure.to_string()))?;
        let builder =
            if self.request_cert { builder.with_client_cert_verifier(trust.clone()) } else { builder.with_no_client_auth() };
        let mut config = builder.with_cert_resolver(Arc::new(resolver));
        config.alpn_protocols = self.alpn.clone();
        let built = (TlsAcceptor::from(Arc::new(config)), trust);
        self.built = Some(built.clone());
        Ok(built)
    }
}

#[derive(Default)]
pub(crate) struct ServerState {
    pub(crate) address: Option<SocketAddr>,
    pub(crate) max_connections: Option<f64>,
    pub(crate) drop_max_connection: bool,
    accept: Option<AbortHandle>,
}

impl ServerState {
    pub(crate) fn listening(&self) -> bool {
        self.accept.is_some()
    }
}

pub(crate) struct ServerShared {
    pub(crate) events: Events,
    pub(crate) setup: Mutex<ServerSetup>,
    pub(crate) state: Mutex<ServerState>,
    /// Connections accepted and not yet closed.
    pub(crate) live: watch::Sender<usize>,
}

impl ServerShared {
    pub(crate) fn new(events: Events, setup: ServerSetup) -> Arc<Self> {
        Arc::new(Self { events, setup: Mutex::new(setup), state: Mutex::default(), live: watch::channel(0).0 })
    }

    /// Bind, announce `listening`, and accept until closed.
    pub(crate) async fn listen(
        self: &Arc<Self>,
        host: Option<&str>,
        port: u16,
        registry: Arc<Mutex<Registry>>,
        jobs: mpsc::UnboundedSender<Job>,
    ) -> Result<(), Value> {
        if lock(&self.state).listening() {
            return Err(error(Some("ERR_SERVER_ALREADY_LISTEN"), "Listen method has been called more than once without closing."));
        }
        let listener = http::bind_tcp(host, port).await.map_err(http_error)?;
        let local = listener.local_addr().map_err(|failure| http_error(http::socket_error(&failure, "listen", None)))?;
        let server = self.clone();
        let task = tokio::spawn(server.accept(listener, registry, jobs));
        {
            let mut state = lock(&self.state);
            state.address = Some(local);
            state.accept = Some(task.abort_handle());
        }
        self.events.send("listening", address(Some(local)));
        Ok(())
    }

    async fn accept(self: Arc<Self>, listener: TcpListener, registry: Arc<Mutex<Registry>>, jobs: mpsc::UnboundedSender<Job>) {
        loop {
            let (tcp, remote) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(failure) => {
                    self.events.send("error", http_error(http::socket_error(&failure, "accept", None)));
                    continue;
                }
            };
            let (max, drop) = {
                let state = lock(&self.state);
                (state.max_connections, state.drop_max_connection)
            };
            if max.is_some_and(|max| *self.live.borrow() as f64 >= max) {
                if drop {
                    let local = tcp.local_addr().ok();
                    self.events.send(
                        "drop",
                        object([
                            ("localAddress", local.map(|local| local.ip().to_string()).into()),
                            ("localPort", local.map(|local| local.port()).into()),
                            ("localFamily", local.as_ref().map(family).into()),
                            ("remoteAddress", remote.ip().to_string().into()),
                            ("remotePort", remote.port().into()),
                            ("remoteFamily", family(&remote).into()),
                        ]),
                    );
                }
                continue;
            }
            let built = lock(&self.setup).acceptor();
            let (acceptor, trust) = match built {
                Ok(built) => built,
                Err(failure) => {
                    self.events.send("error", failure);
                    continue;
                }
            };
            self.live.send_modify(|live| *live += 1);
            tokio::spawn(self.clone().accept_one(tcp, acceptor, trust, registry.clone(), jobs.clone()));
        }
    }

    async fn accept_one(
        self: Arc<Self>,
        tcp: TcpStream,
        acceptor: TlsAcceptor,
        trust: Arc<ClientTrust>,
        registry: Arc<Mutex<Registry>>,
        jobs: mpsc::UnboundedSender<Job>,
    ) {
        /// Counts the connection as live until this task ends, however it ends.
        struct Live(Arc<ServerShared>);
        impl Drop for Live {
            fn drop(&mut self) {
                self.0.live.send_modify(|live| *live -= 1);
            }
        }
        let _live = Live(self.clone());
        let info = SocketInfo {
            connecting: true,
            server: true,
            local: tcp.local_addr().ok(),
            remote: tcp.peer_addr().ok(),
            own_certificate: lock(&self.setup).context.cert.as_deref().and_then(|cert| certificates(cert).into_iter().next()),
            ..SocketInfo::default()
        };
        let timeout = lock(&self.setup).handshake_timeout;
        let accepted = match tokio::time::timeout(timeout, acceptor.accept(tcp)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(failure)) => Err(io_error(&failure, "accept", "")),
            Err(_) => Err(error(Some("ERR_TLS_HANDSHAKE_TIMEOUT"), "TLS handshake timeout")),
        };
        let mut info = info;
        let stream = match accepted {
            Ok(stream) => stream,
            Err(failure) => {
                let id = lock(&registry).next_accepted();
                self.events.send("tlsClientError", object([("error", failure), ("socket", object([("id", id.into()), ("state", info.state())]))]));
                return;
            }
        };
        let connection = stream.get_ref().1;
        info.record(connection);
        info.servername = connection.server_name().map(str::to_owned);
        let requested = lock(&self.setup).request_cert;
        if requested {
            let verdict = trust.check(&info.peer_chain);
            info.authorized = verdict.is_ok();
            info.authorization_error = verdict.err().map(|failure| describe(&failure, "").0.to_owned());
        }
        let state = info.state();
        let info = Arc::new(Mutex::new(info));
        // Registered before it is announced, so the guest's first operation on it finds it.
        let (id, commands, cancelled) = {
            let mut registry = lock(&registry);
            let id = registry.next_accepted();
            let (commands, cancelled) = registry.register(id, info.clone());
            (id, commands, cancelled)
        };
        self.events.send("secureConnection", object([("id", id.into()), ("state", state)]));
        serve(TlsStream::Server(stream), info, commands, cancelled, Events::new(id, jobs), String::new()).await;
    }

    /// Stop accepting; `close` and `close-complete` follow once every connection has closed.
    pub(crate) fn close(self: &Arc<Self>, token: Value) {
        let accept = {
            let mut state = lock(&self.state);
            state.address = None;
            state.accept.take()
        };
        let Some(accept) = accept else {
            self.events.send(
                "close-complete",
                object([("token", token), ("error", error(Some("ERR_SERVER_NOT_RUNNING"), "Server is not running."))]),
            );
            return;
        };
        accept.abort();
        let server = self.clone();
        tokio::spawn(async move {
            let mut live = server.live.subscribe();
            let _ = live.wait_for(|live| *live == 0).await;
            server.events.send("close", Value::Undefined);
            server.events.send("close-complete", object([("token", token)]));
        });
    }

    pub(crate) fn connections(&self) -> usize {
        *self.live.borrow()
    }
}

/// Start a `tls.connect` as socket `id`.
pub(crate) fn start_client(
    registry: &Arc<Mutex<Registry>>,
    id: u32,
    plan: Connect,
    own_certificate: Option<CertificateDer<'static>>,
    jobs: mpsc::UnboundedSender<Job>,
) {
    let info = Arc::new(Mutex::new(SocketInfo { connecting: true, own_certificate, ..SocketInfo::default() }));
    let (commands, cancelled) = lock(registry).register(id, info.clone());
    tokio::spawn(run_client(plan, info, commands, cancelled, Events::new(id, jobs)));
}
