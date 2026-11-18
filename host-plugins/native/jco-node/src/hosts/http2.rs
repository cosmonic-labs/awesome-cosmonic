//! `jco:node/http2`: HTTP/2 servers and client sessions over cleartext TCP (h2c), as Jco's direct
//! Node provider (`http2-host-node.ts`) offers them.
//!
//! The contract is buffered on both sides, so this is too: a server reads a stream's whole request
//! before handing it to the guest's `stream-listener`, and a client stream returns the whole
//! response from `finish`. TLS sessions (`secure`, `https:`) need `jco:node/tls`, which this host
//! does not provide; they are refused with Jco's code for that.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use bytes::Bytes;
use h2::{RecvStream, SendStream, client::SendRequest, server::SendResponse};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::oneshot,
    task::JoinHandle,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::http2::{
        ClientOptions, Errno, Error, Header, Host, HostClientSession, HostClientStream, HostServer, Http2Settings,
        ListenOptions, PingResponse, RequestOptions, Response, ServerAddress, ServerOptions, SessionInfo, StreamState,
        TcpAddress,
    },
    callbacks::Job,
    hosts::{denied_message, http},
    plugin::NodeView,
    state::{Capability, lock},
};

type Http2Result<T> = wasmtime::Result<Result<T, Error>>;

fn plain(code: &str, message: impl Into<String>) -> Error {
    Error {
        name: "Error".into(),
        message: message.into(),
        code: Some(code.into()),
        errno: None,
        syscall: None,
        hostname: None,
        address: None,
        port: None,
    }
}

fn from_http(error: crate::bindings::jco::node::http::Error) -> Error {
    use crate::bindings::jco::node::http::Errno as HttpErrno;
    Error {
        name: error.name,
        message: error.message,
        code: error.code,
        errno: error.errno.map(|errno| match errno {
            HttpErrno::Number(number) => Errno::Number(number),
            HttpErrno::Symbolic(symbol) => Errno::Symbolic(symbol),
        }),
        syscall: error.syscall,
        hostname: error.hostname,
        address: error.address,
        port: error.port,
    }
}

/// A protocol failure, coded as Node codes its HTTP/2 errors.
fn protocol(error: h2::Error) -> Error {
    if let Some(io) = error.get_io() {
        return from_http(http::socket_error(io, "read", None));
    }
    match error.reason() {
        Some(reason) => plain("ERR_HTTP2_STREAM_ERROR", format!("Stream closed with error code {reason:?}")),
        None => plain("ERR_HTTP2_ERROR", error.to_string()),
    }
}

/// Whether a session ending with `error` is one Node reports as `sessionError`: a protocol
/// violation is, a peer closing its connection or saying goodbye is not.
fn reportable(error: &h2::Error) -> bool {
    if let Some(io) = error.get_io() {
        return !matches!(
            io.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        );
    }
    !(error.is_remote() && error.reason() == Some(h2::Reason::NO_ERROR))
}

/// A session-level failure, coded as Node codes them.
fn session_error(error: h2::Error) -> Error {
    if let Some(io) = error.get_io() {
        return from_http(http::socket_error(io, "read", None));
    }
    plain("ERR_HTTP2_ERROR", match error.reason() {
        Some(reason) => format!("Protocol error: {reason:?}"),
        None => error.to_string(),
    })
}

fn tls_required() -> Error {
    plain(
        "ERR_JCO_TLS_ADAPTER_REQUIRED",
        "HTTP/2 over TLS needs a secure context from jco:node/tls: grant tls to the workload",
    )
}

// ---- TLS --------------------------------------------------------------------------------------

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

fn tls_failure(error: impl std::fmt::Display) -> Error {
    plain("ERR_SSL_INVALID_CONTEXT", error.to_string())
}

/// A server's TLS setup from its secure context, offering only HTTP/2 as Node's secure HTTP/2
/// servers do.
fn server_tls(context: &crate::hosts::tls::SecureContext) -> Result<TlsAcceptor, Error> {
    let certs = context.cert.as_deref().map(certificates).unwrap_or_default();
    let key = context
        .key
        .as_deref()
        .and_then(private_key)
        .ok_or_else(|| plain("ERR_SSL_NO_PRIVATE_KEY", "A secure HTTP/2 server needs a private key"))?;
    let mut config = ServerConfig::builder_with_provider(crypto())
        .with_safe_default_protocol_versions()
        .map_err(tls_failure)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(tls_failure)?;
    config.alpn_protocols = vec![b"h2".to_vec()];
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

/// A client's TLS setup: the context's CAs (or the public web roots), unless it turns
/// verification off.
fn client_tls(context: Option<&crate::hosts::tls::SecureContext>) -> Result<TlsConnector, Error> {
    let provider = crypto();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(tls_failure)?;
    let mut config = if context.and_then(|context| context.reject_unauthorized) == Some(false) {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider))).with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        let authorities: Vec<_> =
            context.map(|context| context.ca.iter().flat_map(|ca| certificates(ca)).collect()).unwrap_or_default();
        if authorities.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        } else {
            for authority in authorities {
                let _ = roots.add(authority);
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

/// The settings Node advertises by default, which this host also uses.
fn default_settings() -> Http2Settings {
    Http2Settings {
        header_table_size: Some(4096),
        enable_push: Some(false),
        initial_window_size: Some(65_535),
        max_frame_size: Some(16_384),
        max_concurrent_streams: Some(u32::MAX),
        max_header_list_size: Some(65_535),
        enable_connect_protocol: Some(false),
        custom_settings: Vec::new(),
    }
}

fn header_map(fields: &[Header]) -> Result<::http::HeaderMap, Error> {
    let mut map = ::http::HeaderMap::new();
    for field in fields.iter().filter(|field| !field.name.starts_with(':')) {
        let name = ::http::HeaderName::from_bytes(field.name.to_ascii_lowercase().as_bytes())
            .map_err(|_| plain("ERR_INVALID_HTTP_TOKEN", format!("Header name must be a valid HTTP token [\"{}\"]", field.name)))?;
        let value = ::http::HeaderValue::from_bytes(&field.value)
            .map_err(|_| plain("ERR_INVALID_CHAR", format!("Invalid character in header content [\"{}\"]", field.name)))?;
        map.append(name, value);
    }
    Ok(map)
}

fn pseudo<'a>(fields: &'a [Header], name: &str) -> Option<&'a [u8]> {
    fields.iter().find(|field| field.name == name).map(|field| field.value.as_slice())
}

fn fields_of(map: &::http::HeaderMap) -> Vec<Header> {
    map.iter().map(|(name, value)| Header { name: name.as_str().into(), value: value.as_bytes().to_vec() }).collect()
}

/// A whole body, releasing flow-control capacity as it arrives, then its trailers.
async fn read_body(mut body: RecvStream) -> Result<(Vec<u8>, Vec<Header>), h2::Error> {
    let mut data = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        let _ = body.flow_control().release_capacity(chunk.len());
        data.extend_from_slice(&chunk);
    }
    let trailers = body.trailers().await?.map(|trailers| fields_of(&trailers)).unwrap_or_default();
    Ok((data, trailers))
}

// ---- Server -----------------------------------------------------------------------------------

/// One request stream for a server's `stream-listener`.
pub struct IncomingStream {
    pub(crate) listener: u32,
    pub(crate) session_id: u32,
    pub(crate) id: u32,
    pub(crate) headers: Vec<Header>,
    pub(crate) body: Vec<u8>,
    pub(crate) remote: Option<SocketAddr>,
    pub(crate) reply: oneshot::Sender<StreamReply>,
}

/// What the guest answered: headers, trailers and body, or the message of its failure.
pub(crate) type StreamReply = Result<(Vec<Header>, Vec<Header>, Vec<u8>), String>;

struct ServerShared {
    listener: u32,
    error_listener: u32,
    /// The secure context the server was constructed with, for `secure` servers.
    context: Option<Arc<crate::hosts::tls::SecureContext>>,
    options: ServerOptions,
    settings: Mutex<Http2Settings>,
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
    address: Mutex<Option<ServerAddress>>,
    accept: Mutex<Option<JoinHandle<()>>>,
    next_session: AtomicU32,
}

pub struct Http2Server {
    shared: Arc<ServerShared>,
}

impl Drop for Http2Server {
    fn drop(&mut self) {
        if let Some(accept) = lock(&self.shared.accept).take() {
            accept.abort();
        }
    }
}

fn server_builder(settings: &Http2Settings) -> h2::server::Builder {
    let mut builder = h2::server::Builder::new();
    if let Some(size) = settings.header_table_size {
        builder.header_table_size(size);
    }
    if let Some(size) = settings.initial_window_size {
        builder.initial_window_size(size);
    }
    if let Some(size) = settings.max_frame_size {
        builder.max_frame_size(size.clamp(16_384, 16_777_215));
    }
    if let Some(streams) = settings.max_concurrent_streams {
        builder.max_concurrent_streams(streams);
    }
    if let Some(size) = settings.max_header_list_size {
        builder.max_header_list_size(size);
    }
    if settings.enable_connect_protocol == Some(true) {
        builder.enable_connect_protocol();
    }
    builder
}

async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    shared: Arc<ServerShared>,
    socket: S,
    remote: SocketAddr,
) {
    let builder = server_builder(&lock(&shared.settings).clone());
    let session_id = shared.next_session.fetch_add(1, Ordering::SeqCst) + 1;
    let mut connection = match builder.handshake::<_, Bytes>(socket).await {
        Ok(connection) => connection,
        Err(error) => {
            if reportable(&error) {
                let _ = shared.jobs.send(Job::Http2ServerError { listener: shared.error_listener, error: session_error(error) });
            }
            return;
        }
    };
    while let Some(next) = connection.accept().await {
        match next {
            Ok((request, respond)) => {
                tokio::spawn(answer(shared.clone(), session_id, remote, request, respond));
            }
            Err(error) => {
                if reportable(&error) {
                    let _ = shared.jobs.send(Job::Http2ServerError { listener: shared.error_listener, error: session_error(error) });
                }
                break;
            }
        }
    }
}

async fn answer(
    shared: Arc<ServerShared>,
    session_id: u32,
    remote: SocketAddr,
    request: ::http::Request<RecvStream>,
    mut respond: SendResponse<Bytes>,
) {
    let id = respond.stream_id().as_u32();
    let (parts, body) = request.into_parts();
    let Ok((body, _trailers)) = read_body(body).await else { return };
    // Node's raw headers put the pseudo-headers first.
    let mut headers = vec![Header { name: ":method".into(), value: parts.method.as_str().as_bytes().to_vec() }];
    if let Some(path) = parts.uri.path_and_query() {
        headers.push(Header { name: ":path".into(), value: path.as_str().as_bytes().to_vec() });
    }
    if let Some(scheme) = parts.uri.scheme_str() {
        headers.push(Header { name: ":scheme".into(), value: scheme.as_bytes().to_vec() });
    }
    if let Some(authority) = parts.uri.authority() {
        headers.push(Header { name: ":authority".into(), value: authority.as_str().as_bytes().to_vec() });
    }
    headers.extend(fields_of(&parts.headers));
    let (reply, answered) = oneshot::channel();
    let incoming = IncomingStream {
        listener: shared.listener,
        session_id,
        id,
        headers,
        body,
        remote: Some(remote),
        reply,
    };
    if shared.jobs.send(Job::Http2Stream(Box::new(incoming))).is_err() {
        return;
    }
    let (status, headers, trailers, body) = match answered.await {
        Ok(Ok((headers, trailers, body))) => {
            let status = pseudo(&headers, ":status")
                .and_then(|status| std::str::from_utf8(status).ok()?.parse::<u16>().ok())
                .unwrap_or(200);
            (status, headers, trailers, body)
        }
        // As Jco's provider does for a listener that throws.
        Ok(Err(message)) => (
            500,
            vec![Header { name: "content-type".into(), value: b"text/plain; charset=utf-8".to_vec() }],
            Vec::new(),
            message.into_bytes(),
        ),
        Err(_) => return,
    };
    let Ok(map) = header_map(&headers) else {
        respond.send_reset(h2::Reason::INTERNAL_ERROR);
        return;
    };
    let mut response = ::http::Response::new(());
    *response.status_mut() = ::http::StatusCode::from_u16(status).unwrap_or(::http::StatusCode::OK);
    *response.headers_mut() = map;
    let end = body.is_empty() && trailers.is_empty();
    let Ok(mut stream) = respond.send_response(response, end) else { return };
    if end {
        return;
    }
    let _ = send_all(&mut stream, body, trailers.is_empty()).await;
    if !trailers.is_empty() {
        if let Ok(trailers) = header_map(&trailers) {
            let _ = stream.send_trailers(trailers);
        }
    }
}

/// Send `body` as flow control allows, ending the stream after it when `end`.
async fn send_all(stream: &mut SendStream<Bytes>, body: Vec<u8>, end: bool) -> Result<(), h2::Error> {
    let mut body = Bytes::from(body);
    if body.is_empty() {
        return if end { stream.send_data(Bytes::new(), true) } else { Ok(()) };
    }
    while !body.is_empty() {
        stream.reserve_capacity(body.len());
        let granted = match std::future::poll_fn(|context| stream.poll_capacity(context)).await {
            Some(granted) => granted?,
            None => return Err(h2::Error::from(h2::Reason::STREAM_CLOSED)),
        };
        if granted == 0 {
            continue;
        }
        let chunk = body.split_to(granted.min(body.len()));
        stream.send_data(chunk, end && body.is_empty())?;
    }
    Ok(())
}

fn tcp_address(address: SocketAddr) -> ServerAddress {
    ServerAddress::Tcp(TcpAddress {
        address: address.ip().to_string(),
        family: if address.is_ipv4() { "IPv4" } else { "IPv6" }.into(),
        port: address.port(),
    })
}

// ---- Client -----------------------------------------------------------------------------------

struct Connected {
    send: SendRequest<Bytes>,
    ping: Option<h2::PingPong>,
    connection: JoinHandle<()>,
}

pub struct ClientSession {
    encrypted: bool,
    scheme: ::http::uri::Scheme,
    authority: ::http::uri::Authority,
    settings: Http2Settings,
    connecting: Option<JoinHandle<Result<Connected, Error>>>,
    state: Result<Option<Connected>, Error>,
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        if let Some(connecting) = self.connecting.take() {
            connecting.abort();
        }
        if let Ok(Some(connected)) = &self.state {
            connected.connection.abort();
        }
    }
}

impl ClientSession {
    /// The connection, once established: `Err` if it could not be.
    async fn connected(&mut self) -> Result<&mut Connected, Error> {
        if let Some(connecting) = self.connecting.take() {
            self.state = match connecting.await {
                Ok(result) => result.map(Some),
                Err(_) => Err(plain("ERR_HTTP2_ERROR", "the connection attempt was cancelled")),
            };
        }
        match &mut self.state {
            Ok(Some(connected)) => Ok(connected),
            Ok(None) => Err(plain("ERR_HTTP2_INVALID_SESSION", "The session has been destroyed")),
            Err(error) => Err(error.clone()),
        }
    }
}

async fn connect(host: String, port: u16, settings: Http2Settings, tls: Option<TlsConnector>) -> Result<Connected, Error> {
    let addresses = http::resolve(&host, port).await.map_err(from_http)?;
    let socket = http::connect(&addresses, None).await.map_err(from_http)?;
    let _ = socket.set_nodelay(true);
    match tls {
        None => handshake(socket, settings).await,
        Some(connector) => {
            let name = ServerName::try_from(host.clone())
                .map_err(|_| plain("ERR_TLS_CERT_ALTNAME_INVALID", format!("invalid server name {host}")))?;
            let socket = connector
                .connect(name, socket)
                .await
                .map_err(|error| from_http(http::socket_error(&error, "connect", addresses.first().copied())))?;
            handshake(socket, settings).await
        }
    }
}

async fn handshake<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(socket: S, settings: Http2Settings) -> Result<Connected, Error> {
    let mut builder = h2::client::Builder::new();
    if let Some(size) = settings.initial_window_size {
        builder.initial_window_size(size);
    }
    if let Some(size) = settings.max_frame_size {
        builder.max_frame_size(size.clamp(16_384, 16_777_215));
    }
    if let Some(size) = settings.header_table_size {
        builder.header_table_size(size);
    }
    if let Some(size) = settings.max_header_list_size {
        builder.max_header_list_size(size);
    }
    builder.enable_push(false);
    let (send, mut connection) = builder.handshake::<_, Bytes>(socket).await.map_err(protocol)?;
    let ping = connection.ping_pong();
    let connection = tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(Connected { send, ping, connection })
}

pub struct ClientStream {
    id: u32,
    response: Option<h2::client::ResponseFuture>,
    send: Option<SendStream<Bytes>>,
    ended: bool,
    result: Option<Result<Response, Error>>,
}

// ---- Bindings ---------------------------------------------------------------------------------

impl NodeView<'_> {
    fn http2_allowed(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Http2) {
            Ok(())
        } else {
            Err(plain(&Capability::Http2.denied_code(), denied_message(Capability::Http2)))
        }
    }
}

impl Host for NodeView<'_> {}

impl HostServer for NodeView<'_> {
    async fn new(&mut self, options: ServerOptions, listener: u32, error_listener: u32) -> wasmtime::Result<Resource<Http2Server>> {
        let settings = options.settings.clone();
        // Jco releases the secure context as soon as the server is constructed, so it is
        // resolved now rather than at `listen`.
        let context = options.tls_context.and_then(|id| self.secure_context(id));
        let shared = Arc::new(ServerShared {
            listener,
            error_listener,
            context,
            options,
            settings: Mutex::new(settings),
            jobs: self.node.jobs.clone(),
            address: Mutex::new(None),
            accept: Mutex::new(None),
            next_session: AtomicU32::new(0),
        });
        Ok(self.table.push(Http2Server { shared })?)
    }

    async fn listen(&mut self, self_: Resource<Http2Server>, options: ListenOptions) -> Http2Result<ServerAddress> {
        if let Err(error) = self.http2_allowed() {
            return Ok(Err(error));
        }
        let shared = self.table.get(&self_)?.shared.clone();
        let acceptor = if shared.options.secure {
            let Some(context) = shared.context.as_deref() else {
                return Ok(Err(tls_required()));
            };
            match server_tls(context) {
                Ok(acceptor) => Some(acceptor),
                Err(error) => return Ok(Err(error)),
            }
        } else {
            None
        };
        if lock(&shared.accept).is_some() {
            return Ok(Err(plain(
                "ERR_SERVER_ALREADY_LISTEN",
                "Listen method has been called more than once without closing.",
            )));
        }
        if let Some(path) = options.path {
            return Ok(Err(plain(
                "ERR_JCO_UNSUPPORTED_NODE_API",
                format!("listening on the pipe {path} is not supported in a WebAssembly component: use a TCP port"),
            )));
        }
        let listener = match http::bind_tcp(options.host.as_deref(), options.port.unwrap_or(0)).await {
            Ok(listener) => listener,
            Err(error) => return Ok(Err(from_http(error))),
        };
        let address = match listener.local_addr() {
            Ok(address) => tcp_address(address),
            Err(error) => return Ok(Err(from_http(http::socket_error(&error, "listen", None)))),
        };
        *lock(&shared.address) = Some(address.clone());
        let accept_shared = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, remote)) = listener.accept().await else { continue };
                let _ = socket.set_nodelay(true);
                let shared = accept_shared.clone();
                match acceptor.clone() {
                    None => {
                        tokio::spawn(serve_connection(shared, socket, remote));
                    }
                    Some(acceptor) => {
                        tokio::spawn(async move {
                            match acceptor.accept(socket).await {
                                Ok(tls) => serve_connection(shared, tls, remote).await,
                                Err(error) => {
                                    let error = from_http(http::socket_error(&error, "read", None));
                                    let _ = shared.jobs.send(Job::Http2ServerError { listener: shared.error_listener, error });
                                }
                            }
                        });
                    }
                }
            }
        });
        *lock(&shared.accept) = Some(task);
        Ok(Ok(address))
    }

    async fn close(&mut self, self_: Resource<Http2Server>) -> Http2Result<bool> {
        let shared = self.table.get(&self_)?.shared.clone();
        let accept = lock(&shared.accept).take();
        let was_listening = accept.is_some();
        if let Some(accept) = accept {
            accept.abort();
        }
        *lock(&shared.address) = None;
        // Open sessions finish on their own, as Node's `server.close` lets them.
        let _ = shared.jobs.send(Job::Http2Retire { listener: shared.listener, error_listener: shared.error_listener });
        Ok(Ok(was_listening))
    }

    async fn address(&mut self, self_: Resource<Http2Server>) -> wasmtime::Result<Option<ServerAddress>> {
        Ok(lock(&self.table.get(&self_)?.shared.address).clone())
    }

    async fn update_settings(&mut self, self_: Resource<Http2Server>, settings: Http2Settings) -> Http2Result<()> {
        // New sessions use them; h2 does not renegotiate settings on open ones.
        *lock(&self.table.get(&self_)?.shared.settings) = settings;
        Ok(Ok(()))
    }

    async fn ref_(&mut self, _self_: Resource<Http2Server>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn unref(&mut self, _self_: Resource<Http2Server>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<Http2Server>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostClientSession for NodeView<'_> {
    async fn new(&mut self, authority: String, options: ClientOptions) -> wasmtime::Result<Resource<ClientSession>> {
        let settings = options.settings.clone();
        let parsed = authority.parse::<::http::Uri>().ok();
        let scheme = parsed.as_ref().and_then(|uri| uri.scheme().cloned()).unwrap_or(::http::uri::Scheme::HTTP);
        let host_authority = parsed.as_ref().and_then(|uri| uri.authority().cloned());
        let encrypted = scheme == ::http::uri::Scheme::HTTPS;
        let mut session = ClientSession {
            encrypted,
            scheme: scheme.clone(),
            authority: host_authority.clone().unwrap_or_else(|| ::http::uri::Authority::from_static("localhost")),
            settings: settings.clone(),
            connecting: None,
            state: Ok(None),
        };
        let tls = if encrypted {
            let context = options.tls_context.and_then(|id| self.secure_context(id));
            Some(client_tls(context.as_deref()))
        } else {
            None
        };
        session.state = if let Err(error) = self.http2_allowed() {
            Err(error)
        } else if let Some(Err(error)) = tls {
            Err(error)
        } else if let Some(authority) = host_authority {
            let (host, port) = http::split_authority(authority.as_str(), if encrypted { 443 } else { 80 });
            let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
            session.connecting = Some(tokio::spawn(connect(host, port, settings, tls.and_then(Result::ok))));
            Ok(None)
        } else {
            Err(plain("ERR_INVALID_URL", format!("Invalid URL: {authority}")))
        };
        Ok(self.table.push(session)?)
    }

    async fn ready(&mut self, self_: Resource<ClientSession>) -> Http2Result<SessionInfo> {
        let session = self.table.get_mut(&self_)?;
        let local = session.settings.clone();
        let encrypted = session.encrypted;
        Ok(session.connected().await.map(|_| SessionInfo {
            alpn_protocol: Some(if encrypted { "h2" } else { "h2c" }.into()),
            encrypted,
            local_settings: Http2Settings { custom_settings: local.custom_settings.clone(), ..default_settings() },
            remote_settings: default_settings(),
        }))
    }

    async fn request(
        &mut self,
        self_: Resource<ClientSession>,
        headers: Vec<Header>,
        options: RequestOptions,
    ) -> Http2Result<Resource<ClientStream>> {
        let session = self.table.get_mut(&self_)?;
        let scheme = session.scheme.clone();
        let default_authority = session.authority.clone();
        let started = async {
            let method = pseudo(&headers, ":method").unwrap_or(b"GET");
            let method = ::http::Method::from_bytes(method).map_err(|_| plain("ERR_HTTP2_INVALID_HEADER_VALUE", "Invalid :method"))?;
            let path = pseudo(&headers, ":path").map(|path| String::from_utf8_lossy(path).into_owned()).unwrap_or_else(|| "/".into());
            let authority = pseudo(&headers, ":authority")
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .unwrap_or_else(|| default_authority.to_string());
            let scheme = pseudo(&headers, ":scheme").map(|value| String::from_utf8_lossy(value).into_owned()).unwrap_or_else(|| scheme.to_string());
            let uri = ::http::Uri::builder()
                .scheme(scheme.as_str())
                .authority(authority.as_str())
                .path_and_query(path.as_str())
                .build()
                .map_err(|error| plain("ERR_HTTP2_INVALID_HEADER_VALUE", error.to_string()))?;
            let mut request = ::http::Request::new(());
            *request.method_mut() = method;
            *request.uri_mut() = uri;
            *request.headers_mut() = header_map(&headers)?;
            let end = options.end_stream == Some(true);
            let connected = session.connected().await?;
            let mut send = connected.send.clone().ready().await.map_err(protocol)?;
            let (response, stream) = send.send_request(request, end).map_err(protocol)?;
            Ok::<_, Error>(ClientStream {
                id: response.stream_id().as_u32(),
                response: Some(response),
                send: Some(stream),
                ended: end,
                result: None,
            })
        };
        Ok(match started.await {
            Ok(stream) => Ok(self.table.push(stream)?),
            Err(error) => Err(error),
        })
    }

    async fn close(&mut self, self_: Resource<ClientSession>) -> Http2Result<()> {
        let session = self.table.get_mut(&self_)?;
        if let Some(connecting) = session.connecting.take() {
            connecting.abort();
        }
        // Dropping the request handle lets the connection end once its open streams finish.
        if let Ok(Some(connected)) = std::mem::replace(&mut session.state, Ok(None)) {
            drop(connected.send);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), connected.connection).await;
        }
        Ok(Ok(()))
    }

    async fn destroy(&mut self, self_: Resource<ClientSession>, _code: u32) -> Http2Result<()> {
        let session = self.table.get_mut(&self_)?;
        if let Some(connecting) = session.connecting.take() {
            connecting.abort();
        }
        if let Ok(Some(connected)) = std::mem::replace(&mut session.state, Ok(None)) {
            connected.connection.abort();
        }
        Ok(Ok(()))
    }

    async fn settings(&mut self, self_: Resource<ClientSession>, settings: Http2Settings) -> Http2Result<Http2Settings> {
        // h2 settles settings at the handshake; report them back as Node reports the ones it
        // sent.
        let session = self.table.get_mut(&self_)?;
        Ok(session.connected().await.map(|_| settings))
    }

    async fn ping(&mut self, self_: Resource<ClientSession>, payload: Vec<u8>) -> Http2Result<PingResponse> {
        let session = self.table.get_mut(&self_)?;
        let connected = match session.connected().await {
            Ok(connected) => connected,
            Err(error) => return Ok(Err(error)),
        };
        let Some(ping) = connected.ping.as_mut() else {
            return Ok(Err(plain("ERR_HTTP2_PING_CANCEL", "HTTP2 ping cancelled")));
        };
        let started = Instant::now();
        Ok(match ping.ping(h2::Ping::opaque()).await {
            Ok(_) => Ok(PingResponse { duration_ms: started.elapsed().as_secs_f64() * 1000.0, payload }),
            Err(error) => Err(protocol(error)),
        })
    }

    async fn goaway(&mut self, self_: Resource<ClientSession>, _code: u32, _last: Option<u32>, _data: Vec<u8>) -> Http2Result<()> {
        // A client cannot send GOAWAY through h2; closing has the same effect on new streams.
        HostClientSession::close(self, self_).await
    }

    async fn ref_(&mut self, _self_: Resource<ClientSession>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn unref(&mut self, _self_: Resource<ClientSession>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<ClientSession>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostClientStream for NodeView<'_> {
    async fn write(&mut self, self_: Resource<ClientStream>, chunk: Vec<u8>) -> Http2Result<bool> {
        let stream = self.table.get_mut(&self_)?;
        if stream.ended {
            return Ok(Err(plain("ERR_STREAM_WRITE_AFTER_END", "write after end")));
        }
        let Some(send) = stream.send.as_mut() else {
            return Ok(Err(plain("ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed")));
        };
        Ok(send_all(send, chunk, false).await.map(|()| true).map_err(protocol))
    }

    async fn finish(&mut self, self_: Resource<ClientStream>) -> Http2Result<Response> {
        let stream = self.table.get_mut(&self_)?;
        if let Some(result) = &stream.result {
            return Ok(result.clone());
        }
        if !stream.ended {
            stream.ended = true;
            if let Some(send) = stream.send.as_mut() {
                if let Err(error) = send.send_data(Bytes::new(), true) {
                    return Ok(Err(protocol(error)));
                }
            }
        }
        let Some(response) = stream.response.take() else {
            return Ok(Err(plain("ERR_HTTP2_INVALID_STREAM", "The stream has been destroyed")));
        };
        let result = async {
            let response = response.await.map_err(protocol)?;
            let (parts, body) = response.into_parts();
            let mut headers = vec![Header { name: ":status".into(), value: parts.status.as_str().as_bytes().to_vec() }];
            headers.extend(fields_of(&parts.headers));
            let (body, trailers) = read_body(body).await.map_err(protocol)?;
            Ok(Response { headers, trailers, body })
        }
        .await;
        stream.result = Some(result.clone());
        Ok(result)
    }

    async fn close(&mut self, self_: Resource<ClientStream>, code: u32) -> Http2Result<()> {
        let stream = self.table.get_mut(&self_)?;
        if let Some(send) = stream.send.as_mut() {
            send.send_reset(h2::Reason::from(code));
        }
        stream.ended = true;
        stream.response = None;
        Ok(Ok(()))
    }

    async fn id(&mut self, self_: Resource<ClientStream>) -> wasmtime::Result<Option<u32>> {
        Ok(Some(self.table.get(&self_)?.id))
    }

    async fn state(&mut self, self_: Resource<ClientStream>) -> wasmtime::Result<StreamState> {
        let stream = self.table.get(&self_)?;
        let window = stream.send.as_ref().map(|send| send.capacity() as u32);
        Ok(StreamState {
            state: None,
            local_window_size: window,
            local_close: Some(u32::from(stream.ended)),
            remote_close: Some(u32::from(stream.result.is_some())),
        })
    }

    async fn drop(&mut self, rep: Resource<ClientStream>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}
