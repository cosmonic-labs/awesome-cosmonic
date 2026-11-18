//! `jco:node/http`: HTTP/1.1 clients and servers on the host's sockets.
//!
//! Servers hand each request to the guest's `http-callbacks` listener through the instance's
//! job queue ([`crate::callbacks`]), never from inside a guest call. Request bodies and client
//! responses are buffered, as Jco's Node provider buffers them; server responses may stream.
//! What reaches the guest follows Node: raw header names and order, Node's error shapes, and the
//! headers a Node `ServerResponse` adds (`Date`, `Connection`, `Keep-Alive`, chunked framing).

pub mod wire;

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};
use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::http::{
        Errno, Error, Header, Host, HostClientRequest, HostConnection, HostServer, ListenOptions, RequestOptions,
        Response, ServerAddress, ServerOptions, TcpAddress,
    },
    callbacks::Job,
    errno,
    hosts::{tls::SecureContext, tls_config},
    plugin::NodeView,
    state::{Capability, lock},
};

type HttpResult<T> = wasmtime::Result<Result<T, Error>>;

static NEXT_CONNECTION: AtomicU32 = AtomicU32::new(1);

fn adapter_required() -> Error {
    plain(
        "Error",
        "ERR_JCO_HTTP_ADAPTER_REQUIRED",
        "node:http requires an HTTP provider; select --with-nodejs-http-via or map jco:node/http@0.1.0 to an application host",
    )
}

pub(crate) fn plain(name: &str, code: &str, message: impl Into<String>) -> Error {
    Error {
        name: name.into(),
        message: message.into(),
        code: Some(code.into()),
        errno: None,
        syscall: None,
        hostname: None,
        address: None,
        port: None,
    }
}

/// A libuv failure on a socket call, worded as Node words it: `connect ECONNREFUSED 1.2.3.4:5`.
pub(crate) fn socket_error(error: &io::Error, syscall: &str, address: Option<SocketAddr>) -> Error {
    if error.kind() == io::ErrorKind::TimedOut && error.raw_os_error().is_none() {
        return Error {
            message: error.to_string(),
            syscall: Some(syscall.into()),
            ..plain("Error", "ETIMEDOUT", "")
        };
    }
    if error.raw_os_error().is_none() {
        // A protocol failure rather than an OS one: Node's parser and socket hang-ups.
        let code = match error.kind() {
            io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof => "ECONNRESET",
            _ => "HPE_INVALID_CONSTANT",
        };
        return plain("Error", code, error.to_string());
    }
    let uv = errno::from_io(error);
    let message = match address {
        Some(address) => format!("{syscall} {} {}:{}", uv.code, address.ip(), address.port()),
        None => format!("{syscall} {}", uv.code),
    };
    Error {
        name: "Error".into(),
        message,
        code: Some(uv.code.into()),
        errno: Some(Errno::Number(uv.node_errno())),
        syscall: Some(syscall.into()),
        hostname: None,
        address: address.map(|address| address.ip().to_string()),
        port: address.map(|address| address.port()),
    }
}

/// Listen on `host` (or, as Node does without one, the unspecified IPv6 address, which also takes
/// IPv4, falling back to IPv4 alone) and `port`, failing as Node's `listen` does.
pub(crate) async fn bind_tcp(host: Option<&str>, port: u16) -> Result<TcpListener, Error> {
    let hosts = match host {
        Some(host) => vec![host.to_owned()],
        None => vec!["::".to_owned(), "0.0.0.0".to_owned()],
    };
    let mut failure = None;
    for host in hosts {
        let addresses = match resolve(&host, port).await {
            Ok(addresses) => addresses,
            Err(error) => {
                failure = Some(error);
                continue;
            }
        };
        for address in addresses {
            match TcpListener::bind(address).await {
                Ok(listener) => return Ok(listener),
                Err(error) => {
                    let mut error = socket_error(&error, "listen", Some(address));
                    // Node words a listen failure `listen EADDRINUSE: address already in use ::1:80`.
                    if let Some(uv) = error.code.as_deref().and_then(errno::from_code) {
                        error.message =
                            format!("listen {}: {} {}:{}", uv.code, uv.description, address.ip(), address.port());
                    }
                    failure = Some(error);
                }
            }
        }
    }
    Err(failure.unwrap_or_else(|| plain("Error", "EADDRNOTAVAIL", "listen EADDRNOTAVAIL")))
}

fn timed_out(syscall: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, format!("HTTP {syscall} timed out"))
}

fn fields(headers: &[Header]) -> Vec<wire::Header> {
    headers.iter().map(|header| (header.name.clone(), header.value.clone())).collect()
}

fn to_fields(headers: Vec<wire::Header>) -> Vec<Header> {
    headers.into_iter().map(|(name, value)| Header { name, value }).collect()
}

/// `host:port` with Node's defaults, accepting a bracketed IPv6 literal.
pub(crate) fn split_authority(authority: &str, default_port: u16) -> (String, u16) {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']').unwrap_or((rest, ""));
        let port = rest.strip_prefix(':').and_then(|port| port.parse().ok()).unwrap_or(default_port);
        return (host.to_owned(), port);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host.to_owned(), port.parse().unwrap_or(default_port)),
        _ => (authority.to_owned(), default_port),
    }
}

pub(crate) async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, Error> {
    let found = tokio::net::lookup_host((host, port)).await.map(|found| found.collect::<Vec<_>>());
    match found {
        Ok(found) if !found.is_empty() => Ok(found),
        _ => Err(Error {
            errno: Some(Errno::Number(-3008)),
            syscall: Some("getaddrinfo".into()),
            hostname: Some(host.into()),
            ..plain("Error", "ENOTFOUND", format!("getaddrinfo ENOTFOUND {host}"))
        }),
    }
}

/// Connect to the first address that accepts, as Node's `autoSelectFamily` does.
pub(crate) async fn connect(addresses: &[SocketAddr], timeout: Option<u32>) -> Result<TcpStream, Error> {
    let mut last = None;
    for address in addresses {
        let attempt = TcpStream::connect(address);
        let result = match timeout {
            Some(limit) => tokio::time::timeout(Duration::from_millis(u64::from(limit)), attempt)
                .await
                .unwrap_or_else(|_| Err(timed_out("connect"))),
            None => attempt.await,
        };
        match result {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(socket_error(&error, "connect", Some(*address))),
        }
    }
    Err(last.unwrap_or_else(|| plain("Error", "ENOTFOUND", "no address to connect to")))
}

/// A connection `exchange` speaks HTTP/1.1 over: plain TCP or TLS.
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<S: AsyncRead + AsyncWrite + Unpin + Send> Stream for S {}

fn tls_required() -> Error {
    plain(
        "Error",
        "ERR_JCO_TLS_ADAPTER_REQUIRED",
        "HTTPS requires the same jco:node/tls provider passed to createHttpHost(callbacks, tls)",
    )
}

/// A failed client handshake, with the code Node's `tls` gives the same failure.
fn handshake_error(error: &io::Error, servername: &str) -> Error {
    use rustls::{CertificateError, Error as Tls};
    let tls = error.get_ref().and_then(|inner| inner.downcast_ref::<Tls>());
    let (code, message) = match tls {
        Some(Tls::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => (
            "ERR_TLS_CERT_ALTNAME_INVALID",
            format!("Hostname/IP does not match certificate's altnames: Host: {servername}. is not in the cert's altnames"),
        ),
        Some(Tls::InvalidCertificate(CertificateError::UnknownIssuer)) => {
            ("UNABLE_TO_VERIFY_LEAF_SIGNATURE", "unable to verify the first certificate".to_owned())
        }
        // webpki's view of a self-signed certificate nobody trusts.
        Some(Tls::InvalidCertificate(CertificateError::Other(other))) if format!("{other:?}").contains("CaUsedAsEndEntity") => {
            ("DEPTH_ZERO_SELF_SIGNED_CERT", "self-signed certificate".to_owned())
        }
        Some(Tls::InvalidCertificate(CertificateError::Expired | CertificateError::ExpiredContext { .. })) => {
            ("CERT_HAS_EXPIRED", "certificate has expired".to_owned())
        }
        Some(other) => ("ERR_SSL_HANDSHAKE_FAILURE", other.to_string()),
        None => return socket_error(error, "connect", None),
    };
    plain("Error", code, message)
}

async fn with_timeout<T>(limit: Option<u32>, syscall: &str, work: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    match limit {
        Some(limit) => {
            tokio::time::timeout(Duration::from_millis(u64::from(limit)), work).await.unwrap_or_else(|_| Err(timed_out(syscall)))
        }
        None => work.await,
    }
}

/// One buffered client exchange: what `http.request` does once the guest ends the request.
async fn exchange(
    options: RequestOptions,
    connection: Arc<ConnectionShared>,
    context: Option<Arc<SecureContext>>,
) -> Result<Response, Error> {
    let secure = options.scheme == "https";
    if secure && context.is_none() {
        return Err(tls_required());
    }
    let (host, port) = split_authority(&options.authority, if secure { 443 } else { 80 });
    let addresses = resolve(&host, port).await?;
    let tcp = connect(&addresses, options.connect_timeout_ms).await?;
    let peer = tcp.peer_addr().ok();
    let _ = tcp.set_nodelay(true);
    let stream: Box<dyn Stream> = match context {
        None => Box::new(tcp),
        Some(context) => {
            let connector = tls_config::client(Some(&context), &[b"http/1.1"])
                .map_err(|error| plain("Error", error.code, error.message))?;
            let name = context.servername.clone().unwrap_or_else(|| host.clone());
            let server_name = rustls::pki_types::ServerName::try_from(name.clone())
                .map_err(|_| plain("TypeError", "ERR_INVALID_ARG_VALUE", format!("invalid servername {name}")))?;
            let handshake = with_timeout(options.connect_timeout_ms, "connect", connector.connect(server_name, tcp));
            Box::new(handshake.await.map_err(|error| handshake_error(&error, &name))?)
        }
    };
    let (read, mut write) = tokio::io::split(stream);
    connection.connected.store(true, Ordering::SeqCst);
    let mut headers = fields(&options.headers);
    if wire::header(&headers, "connection").is_none() {
        // Node's global agent keeps connections alive since v19.
        headers.push(("Connection".into(), b"keep-alive".to_vec()));
    }
    if !options.body.is_empty()
        && wire::header(&headers, "content-length").is_none()
        && wire::header(&headers, "transfer-encoding").is_none()
    {
        headers.push(("Content-Length".into(), options.body.len().to_string().into_bytes()));
    }
    let mut message = Vec::new();
    wire::write_head(&mut message, &format!("{} {} HTTP/1.1", options.method, options.path_with_query), &headers);
    message.extend_from_slice(&options.body);
    let io_error = |error: io::Error| socket_error(&error, "read", peer);
    let exchange = async {
        write.write_all(&message).await?;
        let mut reader = wire::Reader::new(read);
        let head = with_timeout(options.first_byte_timeout_ms, "request", async {
            loop {
                let head = reader.response_head(wire::MAX_HEADER_SIZE).await?;
                // Informational responses other than a protocol switch precede the real one.
                if (100..200).contains(&head.status) && head.status != 101 {
                    continue;
                }
                return Ok(head);
            }
        })
        .await?;
        let mut framing = wire::response_framing(&options.method, &head)?;
        let mut body = Vec::new();
        loop {
            let chunk = with_timeout(options.between_bytes_timeout_ms, "read", reader.body_chunk(&mut framing)).await?;
            match chunk {
                Some(chunk) => body.extend_from_slice(&chunk),
                None => break,
            }
        }
        Ok::<_, io::Error>(Response {
            status_code: head.status,
            status_message: head.reason,
            http_version: format!("1.{}", head.minor),
            headers: to_fields(head.headers),
            body,
        })
    };
    tokio::select! {
        result = exchange => result.map_err(io_error),
        () = connection.destroyed.notified() => Err(plain("Error", "ECONNRESET", "socket hang up")),
    }
}

// ---- Connections ------------------------------------------------------------------------------

/// What a guest `connection` handle can reach of the socket behind it.
pub(crate) struct ConnectionShared {
    pub id: u32,
    connected: AtomicBool,
    /// Bytes the guest wrote, for the task that owns the socket.
    writes: mpsc::UnboundedSender<Write>,
    destroyed: Notify,
    paused: AtomicBool,
}

pub(crate) enum Write {
    Data(Vec<u8>),
    End,
}

impl ConnectionShared {
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Write>) {
        let (writes, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Self {
            id: NEXT_CONNECTION.fetch_add(1, Ordering::SeqCst),
            connected: AtomicBool::new(false),
            writes,
            destroyed: Notify::new(),
            paused: AtomicBool::new(false),
        });
        (shared, receiver)
    }

    fn destroy(&self) {
        // `notify_one` keeps the permit for a task not yet waiting.
        self.destroyed.notify_one();
        self.destroyed.notify_waiters();
    }
}

/// The guest's handle on a socket.
pub struct HttpConnection {
    pub(crate) shared: Arc<ConnectionShared>,
    /// Whether dropping the handle ends the socket: true for upgraded connections.
    pub(crate) owned: bool,
}

// ---- Server -----------------------------------------------------------------------------------

/// A response the guest produced, on its way to the socket.
pub(crate) struct OutgoingResponse {
    pub status: u16,
    pub message: String,
    pub headers: Vec<wire::Header>,
    pub body: Vec<u8>,
    /// Chunks of a streamed body; `None` ends it.
    pub stream: Option<mpsc::UnboundedReceiver<Option<Vec<u8>>>>,
}

/// What the guest's listener made of a request.
pub(crate) type Reply = Result<OutgoingResponse, String>;

/// A request on its way to the guest's listener.
pub struct IncomingRequest {
    pub(crate) listener: u32,
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) http_version: String,
    pub(crate) headers: Vec<wire::Header>,
    pub(crate) body: Vec<u8>,
    pub(crate) remote: Option<SocketAddr>,
    pub(crate) connection: Arc<ConnectionShared>,
    pub(crate) reply: oneshot::Sender<Reply>,
}

struct ServerShared {
    listener: u32,
    jobs: mpsc::UnboundedSender<Job>,
    /// For a `node:https` server, the secure context it terminates TLS with.
    context: Option<Arc<SecureContext>>,
    options: ServerOptions,
    accept: Mutex<Option<JoinHandle<()>>>,
    address: Mutex<Option<ServerAddress>>,
    connections: Mutex<HashMap<u64, Tracked>>,
    next: AtomicU64,
    /// Requests the guest has not answered yet; `close` waits for them.
    in_flight: AtomicU64,
    settled: Notify,
}

struct Tracked {
    task: tokio::task::AbortHandle,
    idle: Arc<AtomicBool>,
}

pub struct HttpServer {
    shared: Arc<ServerShared>,
}

impl ServerShared {
    fn close_connections(&self, idle_only: bool) {
        let mut connections = lock(&self.connections);
        connections.retain(|_, tracked| {
            if idle_only && !tracked.idle.load(Ordering::SeqCst) {
                return true;
            }
            tracked.task.abort();
            false
        });
    }

    fn stop_listening(&self) -> bool {
        let accept = lock(&self.accept).take();
        *lock(&self.address) = None;
        match accept {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.shared.stop_listening();
        self.shared.close_connections(false);
        let _ = self.shared.jobs.send(Job::HttpRetire { listener: self.shared.listener });
    }
}

pub(crate) fn tcp_address(address: SocketAddr) -> ServerAddress {
    ServerAddress::Tcp(TcpAddress {
        address: address.ip().to_string(),
        family: if address.is_ipv4() { "IPv4" } else { "IPv6" }.into(),
        port: address.port(),
    })
}

/// Serve one connection until it closes or stops being kept alive.
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    shared: Arc<ServerShared>,
    stream: S,
    remote: Option<SocketAddr>,
    idle: Arc<AtomicBool>,
) {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = wire::Reader::new(read);
    let max = shared.options.max_header_size.map_or(wire::MAX_HEADER_SIZE, |size| size as usize);
    loop {
        idle.store(true, Ordering::SeqCst);
        let head = match reader.request_head(max).await {
            Ok(Some(head)) => head,
            Ok(None) => return,
            Err(_) => {
                // llhttp's answer to a request it cannot parse.
                let _ = write.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
                return;
            }
        };
        idle.store(false, Ordering::SeqCst);
        if wire::wants_continue(&head.headers) {
            // Node answers `Expect: 100-continue` itself when no `checkContinue` listener exists.
            if write.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.is_err() {
                return;
            }
        }
        let keep_alive = wire::keep_alive(head.minor, &head.headers) && shared.options.keep_alive != Some(false);
        let body = match wire::request_framing(&head.headers) {
            Ok(framing) => match reader.body(framing).await {
                Ok(body) => body,
                Err(_) => return,
            },
            Err(_) => {
                let _ = write.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").await;
                return;
            }
        };
        let (connection, mut writes) = ConnectionShared::new();
        connection.connected.store(true, Ordering::SeqCst);
        let (reply, response) = oneshot::channel();
        let request = IncomingRequest {
            listener: shared.listener,
            method: head.method.clone(),
            url: head.target,
            http_version: format!("1.{}", head.minor),
            headers: head.headers,
            body,
            remote,
            connection: connection.clone(),
            reply,
        };
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        let answered = async {
            if shared.jobs.send(Job::HttpRequest(Box::new(request))).is_err() {
                return Err("the component is gone".to_owned());
            }
            tokio::select! {
                reply = response => reply.unwrap_or_else(|_| Err("HTTP request was not answered".to_owned())),
                () = connection.destroyed.notified() => Err("destroyed".to_owned()),
            }
        }
        .await;
        let finished = write_response(&mut write, &head.method, head.minor, keep_alive, answered, &mut writes).await;
        if shared.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            shared.settled.notify_waiters();
        }
        if !finished || !keep_alive {
            let _ = write.shutdown().await;
            return;
        }
    }
}

/// Write what the guest answered as Node's `ServerResponse` would, after `writeHead`.
/// `false` when the connection cannot be reused.
async fn write_response<W: AsyncWrite + Unpin>(
    write: &mut W,
    method: &str,
    minor: u8,
    keep_alive: bool,
    answered: Reply,
    guest_writes: &mut mpsc::UnboundedReceiver<Write>,
) -> bool {
    let response = match answered {
        Ok(response) => response,
        Err(message) if message == "destroyed" => return false,
        Err(message) => OutgoingResponse {
            status: 500,
            message: "Internal Server Error".into(),
            headers: vec![("content-type".into(), b"text/plain; charset=utf-8".to_vec())],
            body: message.into_bytes(),
            stream: None,
        },
    };
    // Anything the guest wrote straight to the socket goes first.
    while let Ok(write_) = guest_writes.try_recv() {
        if let Write::Data(data) = write_ {
            if write.write_all(&data).await.is_err() {
                return false;
            }
        }
    }
    let mut headers = response.headers;
    let has_body = !(method.eq_ignore_ascii_case("HEAD") || matches!(response.status, 204 | 304) || response.status < 200);
    if wire::header(&headers, "date").is_none() {
        headers.push(("Date".into(), wire::http_date(SystemTime::now()).into_bytes()));
    }
    let explicit = wire::header(&headers, "content-length").is_some() || wire::header(&headers, "transfer-encoding").is_some();
    // HTTP/1.0 has no chunked framing, so an unframed body there ends with the connection.
    let chunked = has_body && !explicit && minor >= 1;
    let keep_alive = keep_alive && (explicit || chunked || !has_body);
    if wire::header(&headers, "connection").is_none() {
        if keep_alive {
            headers.push(("Connection".into(), b"keep-alive".to_vec()));
            headers.push(("Keep-Alive".into(), b"timeout=5".to_vec()));
        } else {
            headers.push(("Connection".into(), b"close".to_vec()));
        }
    }
    if chunked {
        headers.push(("Transfer-Encoding".into(), b"chunked".to_vec()));
    }
    let reason = if response.message.is_empty() {
        http::StatusCode::from_u16(response.status).ok().and_then(|code| code.canonical_reason()).unwrap_or("unknown")
    } else {
        response.message.as_str()
    };
    let mut head = Vec::new();
    wire::write_head(&mut head, &format!("HTTP/1.1 {} {reason}", response.status), &headers);
    if write.write_all(&head).await.is_err() {
        return false;
    }
    let send = async |write: &mut W, data: &[u8]| -> io::Result<()> {
        if !has_body {
            return Ok(());
        }
        if chunked { wire::write_chunk(write, data).await } else { write.write_all(data).await }
    };
    if send(write, &response.body).await.is_err() {
        return false;
    }
    if let Some(mut stream) = response.stream {
        loop {
            match stream.recv().await {
                Some(Some(chunk)) => {
                    if send(write, &chunk).await.is_err() {
                        return false;
                    }
                }
                Some(None) => break,
                // The body failed: Node destroys the response, so the message never completes.
                None => return false,
            }
        }
    }
    if chunked && write.write_all(b"0\r\n\r\n").await.is_err() {
        return false;
    }
    write.flush().await.is_ok() && keep_alive
}

// ---- Client request resource -------------------------------------------------------------------

pub struct ClientRequest {
    connection: Arc<ConnectionShared>,
    handed_out: bool,
    finish: Option<RequestOptions>,
    result: Arc<Mutex<Option<Result<Response, Error>>>>,
    task: Option<JoinHandle<()>>,
}

impl Drop for ClientRequest {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---- Host implementations ----------------------------------------------------------------------

impl NodeView<'_> {
    fn http_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Http) { Ok(()) } else { Err(adapter_required()) }
    }

    fn http_trap(&self) -> wasmtime::Result<()> {
        if self.node.allows(Capability::Http) {
            return Ok(());
        }
        let error = adapter_required();
        Err(wasmtime::Error::msg(format!("{}: {}", error.code.unwrap_or_default(), error.message)))
    }
}

impl HostClientRequest for NodeView<'_> {
    async fn new(&mut self, options: RequestOptions) -> wasmtime::Result<Resource<ClientRequest>> {
        self.http_trap()?;
        let (connection, _) = ConnectionShared::new();
        let request =
            ClientRequest { connection, handed_out: false, finish: Some(options), result: Arc::default(), task: None };
        Ok(self.table.push(request)?)
    }

    async fn socket(&mut self, self_: Resource<ClientRequest>) -> wasmtime::Result<Option<Resource<HttpConnection>>> {
        let request = self.table.get_mut(&self_)?;
        if request.handed_out {
            return Ok(None);
        }
        // The socket exists from the guest's point of view as soon as it asks; the connect itself
        // happens when the request is finished, as Node's agent assigns a socket before connecting.
        request.handed_out = true;
        let shared = request.connection.clone();
        Ok(Some(self.table.push(HttpConnection { shared, owned: false })?))
    }

    async fn finish(&mut self, self_: Resource<ClientRequest>, headers: Vec<Header>, body: Vec<u8>) -> HttpResult<bool> {
        let request = self.table.get_mut(&self_)?;
        let Some(mut options) = request.finish.take() else {
            return Ok(Err(plain("Error", "ERR_STREAM_WRITE_AFTER_END", "write after end")));
        };
        options.headers = headers;
        options.body = body;
        let result = request.result.clone();
        let connection = request.connection.clone();
        let context = options.tls.as_ref().and_then(|tls| self.secure_context(tls.context_id));
        let task = tokio::spawn(async move {
            let outcome = exchange(options, connection, context).await;
            *lock(&result) = Some(outcome);
        });
        self.table.get_mut(&self_)?.task = Some(task);
        Ok(Ok(true))
    }

    async fn response(&mut self, self_: Resource<ClientRequest>) -> HttpResult<Option<Response>> {
        let request = self.table.get_mut(&self_)?;
        let taken = lock(&request.result).take();
        Ok(match taken {
            None => Ok(None),
            Some(Ok(response)) => Ok(Some(response)),
            Some(Err(error)) => Err(error),
        })
    }

    async fn close(&mut self, self_: Resource<ClientRequest>) -> wasmtime::Result<()> {
        let request = self.table.get_mut(&self_)?;
        request.connection.destroy();
        if let Some(task) = request.task.take() {
            task.abort();
        }
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<ClientRequest>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostConnection for NodeView<'_> {
    async fn id(&mut self, self_: Resource<HttpConnection>) -> wasmtime::Result<u32> {
        Ok(self.table.get(&self_)?.shared.id)
    }

    async fn write(&mut self, self_: Resource<HttpConnection>, data: Vec<u8>) -> HttpResult<bool> {
        let connection = self.table.get(&self_)?;
        Ok(match connection.shared.writes.send(Write::Data(data)) {
            Ok(()) => Ok(!connection.shared.paused.load(Ordering::SeqCst)),
            Err(_) => Err(plain("Error", "ERR_STREAM_DESTROYED", "Cannot call write after a stream was destroyed")),
        })
    }

    async fn end(&mut self, self_: Resource<HttpConnection>) -> wasmtime::Result<()> {
        let _ = self.table.get(&self_)?.shared.writes.send(Write::End);
        Ok(())
    }

    async fn destroy(&mut self, self_: Resource<HttpConnection>) -> wasmtime::Result<()> {
        self.table.get(&self_)?.shared.destroy();
        Ok(())
    }

    async fn pause(&mut self, self_: Resource<HttpConnection>) -> wasmtime::Result<()> {
        self.table.get(&self_)?.shared.paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn resume(&mut self, self_: Resource<HttpConnection>) -> wasmtime::Result<()> {
        self.table.get(&self_)?.shared.paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    // Socket tuning has no effect a guest could observe through this interface.
    async fn set_timeout(&mut self, _self_: Resource<HttpConnection>, _milliseconds: u32) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn set_no_delay(&mut self, _self_: Resource<HttpConnection>, _value: bool) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn set_keep_alive(&mut self, _self_: Resource<HttpConnection>, _value: bool, _delay: u32) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn ref_(&mut self, _self_: Resource<HttpConnection>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn unref(&mut self, _self_: Resource<HttpConnection>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<HttpConnection>) -> wasmtime::Result<()> {
        let connection = self.table.delete(rep)?;
        if connection.owned {
            connection.shared.destroy();
        }
        Ok(())
    }
}

impl HostServer for NodeView<'_> {
    async fn new(&mut self, options: ServerOptions, listener: u32) -> wasmtime::Result<Resource<HttpServer>> {
        self.http_trap()?;
        // Jco releases the secure context once the server is constructed.
        let context = options.tls.as_ref().and_then(|tls| self.secure_context(tls.context_id));
        let shared = Arc::new(ServerShared {
            listener,
            jobs: self.node.jobs.clone(),
            context,
            options,
            accept: Mutex::new(None),
            address: Mutex::new(None),
            connections: Mutex::default(),
            next: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            settled: Notify::new(),
        });
        Ok(self.table.push(HttpServer { shared })?)
    }

    async fn listen(&mut self, self_: Resource<HttpServer>, options: ListenOptions) -> HttpResult<ServerAddress> {
        let shared = self.table.get(&self_)?.shared.clone();
        let acceptor = match (&shared.options.tls, &shared.context) {
            (None, _) => None,
            (Some(_), None) => return Ok(Err(tls_required())),
            (Some(_), Some(context)) => match tls_config::server(context, &[b"http/1.1"]) {
                Ok(acceptor) => Some(acceptor),
                Err(error) => return Ok(Err(plain("Error", error.code, error.message))),
            },
        };
        if lock(&shared.accept).is_some() {
            return Ok(Err(plain(
                "Error",
                "ERR_SERVER_ALREADY_LISTEN",
                "Listen method has been called more than once without closing.",
            )));
        }
        if let Some(path) = options.path {
            return Ok(Err(plain(
                "Error",
                "ERR_JCO_UNSUPPORTED_NODE_API",
                format!("listening on the pipe {path} is not supported in a WebAssembly component: use a TCP port"),
            )));
        }
        let listener = match bind_tcp(options.host.as_deref(), options.port.unwrap_or(0)).await {
            Ok(listener) => listener,
            Err(error) => return Ok(Err(error)),
        };
        let address = match listener.local_addr() {
            Ok(address) => tcp_address(address),
            Err(error) => return Ok(Err(socket_error(&error, "listen", None))),
        };
        *lock(&shared.address) = Some(address.clone());
        let accept_shared = shared.clone();
        let no_delay = shared.options.no_delay.unwrap_or(true);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, remote)) = listener.accept().await else { continue };
                let _ = stream.set_nodelay(no_delay);
                let id = accept_shared.next.fetch_add(1, Ordering::SeqCst);
                let idle = Arc::new(AtomicBool::new(true));
                let server = accept_shared.clone();
                let task_idle = idle.clone();
                let acceptor = acceptor.clone();
                let task = tokio::spawn(async move {
                    match acceptor {
                        None => serve(server.clone(), stream, Some(remote), task_idle).await,
                        // A failed handshake is the client's problem; Node reports it as a
                        // `tlsClientError` the guest's server does not see here.
                        Some(acceptor) => {
                            if let Ok(stream) = acceptor.accept(stream).await {
                                serve(server.clone(), stream, Some(remote), task_idle).await;
                            }
                        }
                    }
                    lock(&server.connections).remove(&id);
                });
                lock(&accept_shared.connections).insert(id, Tracked { task: task.abort_handle(), idle });
            }
        });
        *lock(&shared.accept) = Some(task);
        Ok(Ok(address))
    }

    async fn close(&mut self, self_: Resource<HttpServer>) -> HttpResult<bool> {
        let shared = self.table.get(&self_)?.shared.clone();
        let was_listening = shared.stop_listening();
        // Since Node 19, closing a server also closes its idle keep-alive connections.
        shared.close_connections(true);
        // Requests in flight are the guest's to answer, so `close` cannot wait for them here
        // without blocking the very callbacks that answer them; they finish on their own.
        let _ = shared.jobs.send(Job::HttpRetire { listener: shared.listener });
        Ok(Ok(was_listening))
    }

    async fn close_all_connections(&mut self, self_: Resource<HttpServer>) -> HttpResult<()> {
        self.table.get(&self_)?.shared.close_connections(false);
        Ok(Ok(()))
    }

    async fn close_idle_connections(&mut self, self_: Resource<HttpServer>) -> HttpResult<()> {
        self.table.get(&self_)?.shared.close_connections(true);
        Ok(Ok(()))
    }

    async fn get_connections(&mut self, self_: Resource<HttpServer>) -> HttpResult<u64> {
        Ok(Ok(lock(&self.table.get(&self_)?.shared.connections).len() as u64))
    }

    async fn address(&mut self, self_: Resource<HttpServer>) -> wasmtime::Result<Option<ServerAddress>> {
        Ok(lock(&self.table.get(&self_)?.shared.address).clone())
    }

    // A host has no event loop for a server to keep alive.
    async fn ref_(&mut self, _self_: Resource<HttpServer>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn unref(&mut self, _self_: Resource<HttpServer>) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<HttpServer>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl Host for NodeView<'_> {
    async fn request(&mut self, options: RequestOptions) -> HttpResult<Response> {
        if let Err(error) = self.http_granted() {
            return Ok(Err(error));
        }
        let (connection, _) = ConnectionShared::new();
        // The guest releases the context when this call returns, so it is resolved now.
        let context = options.tls.as_ref().and_then(|tls| self.secure_context(tls.context_id));
        Ok(exchange(options, connection, context).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorities_split_like_url() {
        assert_eq!(split_authority("example.com", 80), ("example.com".into(), 80));
        assert_eq!(split_authority("127.0.0.1:8080", 80), ("127.0.0.1".into(), 8080));
        assert_eq!(split_authority("[::1]:3000", 80), ("::1".into(), 3000));
        assert_eq!(split_authority("[::1]", 443), ("::1".into(), 443));
    }

    #[test]
    fn connect_failures_read_like_node() {
        let error = socket_error(
            &io::Error::from_raw_os_error(libc::ECONNREFUSED),
            "connect",
            Some("127.0.0.1:1".parse().unwrap()),
        );
        assert_eq!(error.message, "connect ECONNREFUSED 127.0.0.1:1");
        assert_eq!((error.address.as_deref(), error.port), (Some("127.0.0.1"), Some(1)));
        assert!(matches!(error.errno, Some(Errno::Number(-111))));
    }
}
