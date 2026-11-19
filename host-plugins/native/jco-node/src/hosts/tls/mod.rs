//! `jco:node/tls`: TLS for the guest's `node:tls`, `node:https` and secure `node:http2`.
//!
//! - `start-tls` upgrades a pair of `wasi:io` streams to a `wasi:tls` client connection, using
//!   the same rustls-backed `wasi:tls@0.2.0-draft` implementation this plugin links.
//! - Secure contexts (`create-context`), which the HTTP and HTTP/2 hosts use as well.
//! - Node's TLS sockets and servers (`connect`, `create-server` and their operations), on rustls:
//!   see [`sockets`]. Their events reach the guest through `jco:node/tls-callbacks`.
//!
//! Values cross as Jco's wire graph ([`wire`]), as with its Node provider.

mod certificate;
mod sockets;
mod wire;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use rustls::pki_types::CertificateDer;
use wash_runtime::wasmtime::{self, component::Resource};
use wasmtime_wasi::p2::{DynInputStream, DynOutputStream};
use wasmtime_wasi_tls::{
    WasiTlsCtxView,
    p2::{HostFutureClientStreams, bindings::types::HostClientHandshake},
};

use self::{
    sockets::{Command, Connect, Events, Registry, ServerSetup, ServerShared},
    wire::{Value, decode, object, strings},
};
use crate::{
    bindings::jco::node::tls::{Error, Host, QueryOperation, ServerOperationKind, SocketOperationKind},
    hosts::{denied_message, tls_config},
    plugin::NodeView,
    state::{Capability, lock},
};

type TlsResult<T> = wasmtime::Result<Result<T, Error>>;

fn coded(code: &str, message: impl Into<String>) -> Error {
    Error { name: "Error".into(), message: message.into(), code: Some(code.into()) }
}

fn unsupported(what: &str, reason: &str) -> Error {
    coded("ERR_JCO_UNSUPPORTED_NODE_API", format!("{what} is not supported by this host: {reason}"))
}

/// A wire error as the interface's error record.
fn to_error(value: Value) -> Error {
    let text = |key: &str| value.get(key).and_then(Value::text).map(str::to_owned);
    Error {
        name: match &value {
            Value::Error(name, _) => name.clone(),
            _ => "Error".into(),
        },
        message: text("message").unwrap_or_default(),
        code: text("code"),
    }
}

fn bad_options() -> Error {
    coded("ERR_INVALID_ARG_VALUE", "the TLS options could not be decoded")
}

/// Key material and trust settings from a guest's `tls.createSecureContext` options.
#[derive(Clone, Default)]
pub(crate) struct SecureContext {
    pub(crate) key: Option<Vec<u8>>,
    pub(crate) cert: Option<Vec<u8>>,
    pub(crate) ca: Vec<Vec<u8>>,
    pub(crate) reject_unauthorized: Option<bool>,
    /// The name a client verifies the server's certificate against (and sends as SNI), when it
    /// is not the host connected to.
    pub(crate) servername: Option<String>,
}

impl SecureContext {
    fn from_options(options: &Value) -> Self {
        SecureContext {
            key: options.get("key").and_then(Value::material),
            cert: options.get("cert").and_then(Value::material),
            ca: options.get("ca").map(Value::materials).unwrap_or_default(),
            reject_unauthorized: options.get("rejectUnauthorized").and_then(Value::boolean),
            servername: options.get("servername").and_then(Value::text).filter(|name| !name.is_empty()).map(str::to_owned),
        }
    }

    fn own_certificate(&self) -> Option<CertificateDer<'static>> {
        self.cert.as_deref().and_then(|cert| tls_config::certificates(cert).into_iter().next())
    }
}

#[derive(Default)]
pub(crate) struct Contexts {
    next: u32,
    contexts: HashMap<u32, Arc<SecureContext>>,
    /// `tls.setDefaultCACertificates`.
    default_ca: Vec<Vec<u8>>,
    registry: Arc<Mutex<Registry>>,
}

impl NodeView<'_> {
    /// The secure context `id`, for the HTTP and HTTP/2 hosts.
    pub(crate) fn secure_context(&self, id: u32) -> Option<Arc<SecureContext>> {
        lock(&self.node.tls_contexts).contexts.get(&id).cloned()
    }

    fn tls_allowed(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Tls) {
            Ok(())
        } else {
            Err(coded(&Capability::Tls.denied_code(), denied_message(Capability::Tls)))
        }
    }

    fn registry(&self) -> Arc<Mutex<Registry>> {
        lock(&self.node.tls_contexts).registry.clone()
    }

    /// The context options name: by `contextId` (a `secureContext`), else their own key material.
    /// Without CAs of its own, it trusts the defaults `setDefaultCACertificates` set.
    fn context_for(&self, options: &Value) -> Result<SecureContext, Error> {
        let contexts = lock(&self.node.tls_contexts);
        let mut context = match options.get("contextId").and_then(Value::number) {
            Some(id) => contexts
                .contexts
                .get(&(id as u32))
                .map(|context| (**context).clone())
                .ok_or_else(|| coded("ERR_INVALID_ARG_VALUE", "the secure context has been released"))?,
            None => SecureContext::from_options(options),
        };
        if context.ca.is_empty() {
            context.ca = contexts.default_ca.clone();
        }
        Ok(context)
    }

    fn certificates(&self, kind: &str) -> Result<Value, Error> {
        let system = || -> Vec<String> {
            rustls_native_certs::load_native_certs().certs.iter().map(|certificate| certificate::pem(certificate)).collect()
        };
        let pems = match kind {
            "default" => {
                let defaults = lock(&self.node.tls_contexts).default_ca.clone();
                if defaults.is_empty() {
                    system()
                } else {
                    defaults.iter().flat_map(|ca| tls_config::certificates(ca)).map(|der| certificate::pem(&der)).collect()
                }
            }
            // Node's bundled store is Mozilla's; this host has no copy of its own, so it lists
            // the system's, which is Mozilla's on most Linux distributions.
            "system" | "bundled" => system(),
            "extra" => Vec::new(),
            other => {
                return Err(Error {
                    name: "TypeError".into(),
                    ..coded("ERR_INVALID_ARG_VALUE", format!("The argument 'type' is invalid. Received '{other}'"))
                });
            }
        };
        Ok(strings(pems))
    }

    async fn socket_operation(&mut self, id: u32, operation: SocketOperationKind, args: Value) -> Result<Value, Error> {
        let registry = self.registry();
        let (info, commands) = {
            let registry = lock(&registry);
            let socket = registry.sockets.get(&id).ok_or_else(|| coded("ERR_SOCKET_CLOSED", "Socket is closed"))?;
            (socket.info.clone(), socket.commands.clone())
        };
        let send = |command: Command| {
            let _ = commands.send(command);
            Ok(Value::Undefined)
        };
        let argument = |index: usize| args.at(index).cloned().unwrap_or(Value::Undefined);
        use SocketOperationKind as Kind;
        match operation {
            Kind::State => Ok(lock(&info).state()),
            Kind::Address => Ok(sockets::address(lock(&info).local)),
            Kind::Certificate => {
                Ok(lock(&info).own_certificate.as_ref().map_or(Value::Object(Vec::new()), |own| {
                    certificate::describe(std::slice::from_ref(own), false)
                }))
            }
            Kind::PeerCertificate => {
                Ok(certificate::describe(&lock(&info).peer_chain, argument(0).boolean().unwrap_or(false)))
            }
            Kind::Cipher => Ok(lock(&info).cipher()),
            Kind::EphemeralKeyInfo => Ok(lock(&info).ephemeral_key()),
            Kind::Protocol => Ok(lock(&info).protocol.map_or(Value::Null, Value::from)),
            Kind::SessionReused => Ok(lock(&info).resumed.into()),
            // rustls keeps sessions and tickets to itself, and doesn't expose Finished messages.
            Kind::Finished | Kind::PeerFinished | Kind::Session | Kind::TlsTicket => Ok(Value::Undefined),
            Kind::SharedSigalgs => Ok(Value::Array(Vec::new())),
            Kind::ExportKeyingMaterial => {
                if lock(&info).connecting {
                    return Err(coded("ERR_TLS_INVALID_STATE", "TLS socket connection must be securely established"));
                }
                let length = argument(0).number().filter(|length| *length >= 0.0 && *length <= 65_536.0);
                let Some(length) = length else {
                    return Err(Error { name: "RangeError".into(), ..coded("ERR_OUT_OF_RANGE", "The value of \"length\" is out of range.") });
                };
                let label = argument(1).material().unwrap_or_default();
                let context = match argument(2) {
                    Value::Undefined | Value::Null => None,
                    other => other.material(),
                };
                let (reply, answer) = tokio::sync::oneshot::channel();
                let _ = commands.send(Command::ExportKeyingMaterial { length: length as usize, label, context, reply });
                match answer.await {
                    Ok(Ok(material)) => Ok(Value::Bytes(material)),
                    Ok(Err(failure)) => Err(coded("ERR_CRYPTO_OPERATION_FAILED", failure)),
                    Err(_) => Err(coded("ERR_SOCKET_CLOSED", "Socket is closed")),
                }
            }
            // rustls never renegotiates, which is what disabling renegotiation asks for.
            Kind::DisableRenegotiation => Ok(Value::Undefined),
            Kind::MaxSendFragment => {
                let size = argument(0).number().unwrap_or(0.0);
                if (512.0..=16_384.0).contains(&size) {
                    let _ = commands.send(Command::MaxFragment(size as usize));
                    Ok(Value::Bool(true))
                } else {
                    Ok(Value::Bool(false))
                }
            }
            Kind::Timeout => send(Command::Timeout(argument(0).number().unwrap_or(0.0).max(0.0) as u32)),
            Kind::NoDelay => send(Command::NoDelay(argument(0).boolean().unwrap_or(true))),
            Kind::KeepAlive => send(Command::KeepAlive(
                argument(0).boolean().unwrap_or(false),
                argument(1).number().unwrap_or(0.0).max(0.0) as u32,
            )),
            Kind::Resume => send(Command::Resume),
            Kind::Pause => send(Command::Pause),
            // The host keeps running for its embedder, whatever the guest's sockets.
            Kind::Ref | Kind::Unref => Ok(Value::Undefined),
            Kind::Destroy => {
                lock(&registry).sockets.remove(&id);
                Ok(Value::Undefined)
            }
            Kind::Renegotiate => {
                // TLS 1.3 has no renegotiation, and rustls does not renegotiate TLS 1.2.
                let token = argument(1);
                Events::new(id, self.node.jobs.clone()).send(
                    "renegotiate",
                    object([
                        ("token", token),
                        ("error", wire::error(Some("ERR_TLS_RENEGOTIATE"), "Attempt to renegotiate TLS session failed")),
                    ]),
                );
                Ok(Value::Bool(false))
            }
            Kind::EnableTrace => Err(unsupported("tls.TLSSocket.enableTrace()", "rustls has no protocol trace")),
            Kind::KeyCert => Err(unsupported("tls.TLSSocket.setKeyCert()", "rustls cannot change a connection's certificate")),
            Kind::SetSession => Err(unsupported("tls.TLSSocket.setSession()", "rustls keeps its sessions to itself")),
            Kind::SetServername => {
                Err(unsupported("tls.TLSSocket.setServername()", "the server name is sent from tls.connect()'s options"))
            }
        }
    }

    async fn server_operation(&mut self, id: u32, operation: ServerOperationKind, args: Value) -> Result<Value, Error> {
        let registry = self.registry();
        let server = lock(&registry).servers.get(&id).cloned().ok_or_else(|| coded("ERR_SERVER_NOT_RUNNING", "Server is not running."))?;
        let argument = |index: usize| args.at(index).cloned().unwrap_or(Value::Undefined);
        use ServerOperationKind as Kind;
        match operation {
            Kind::State => {
                let state = lock(&server.state);
                Ok(object([
                    ("listening", state.listening().into()),
                    ("maxConnections", state.max_connections.into()),
                    ("dropMaxConnection", state.drop_max_connection.into()),
                ]))
            }
            Kind::Address => {
                let local = lock(&server.state).address;
                Ok(if local.is_some() { sockets::address(local) } else { Value::Null })
            }
            Kind::Listen => {
                let options = argument(0);
                if options.get("path").and_then(Value::text).is_some() {
                    return Err(unsupported("tls.Server.listen({ path })", "it listens on TCP only"));
                }
                let port = match options.get("port") {
                    Some(Value::Number(port)) => *port,
                    Some(Value::Text(port)) => port.parse().unwrap_or(-1.0),
                    _ => 0.0,
                };
                if !(0.0..=65_535.0).contains(&port) || port.fract() != 0.0 {
                    return Err(Error {
                        name: "RangeError".into(),
                        ..coded("ERR_SOCKET_BAD_PORT", "Port should be >= 0 and < 65536.")
                    });
                }
                let host = options.get("host").and_then(Value::text).map(str::to_owned);
                server.listen(host.as_deref(), port as u16, registry, self.node.jobs.clone()).await.map_err(to_error)?;
                Ok(Value::Undefined)
            }
            Kind::Close => {
                server.close(argument(0));
                Ok(Value::Undefined)
            }
            Kind::Connections => {
                server.events.send("connections", object([("token", argument(0)), ("count", server.connections().into())]));
                Ok(Value::Undefined)
            }
            Kind::Ref | Kind::Unref => Ok(Value::Undefined),
            Kind::TicketKeys => Ok(Value::Bytes(lock(&server.setup).ticket_keys.clone())),
            Kind::SetTicketKeys => match argument(0) {
                Value::Bytes(keys) if keys.len() == 48 => {
                    lock(&server.setup).ticket_keys = keys;
                    Ok(Value::Undefined)
                }
                _ => Err(Error {
                    name: "TypeError".into(),
                    ..coded("ERR_INVALID_ARG_VALUE", "Session ticket keys must be a 48-byte buffer")
                }),
            },
            Kind::SecureContext => {
                let context = self.context_for(&argument(0))?;
                let mut setup = lock(&server.setup);
                setup.context = context;
                setup.changed();
                Ok(Value::Undefined)
            }
            Kind::AddContext => {
                let hostname = argument(0).text().map(str::to_owned).ok_or_else(|| {
                    Error { name: "TypeError".into(), ..coded("ERR_INVALID_ARG_TYPE", "The \"hostname\" argument must be of type string") }
                })?;
                let context = match argument(1) {
                    Value::Number(context) => self.context_for(&object([("contextId", context.into())]))?,
                    options => self.context_for(&options)?,
                };
                let mut setup = lock(&server.setup);
                setup.named.push((hostname, context));
                setup.changed();
                Ok(Value::Undefined)
            }
            Kind::MaxConnections => {
                lock(&server.state).max_connections = argument(0).number();
                Ok(Value::Undefined)
            }
            Kind::DropMaxConnection => {
                lock(&server.state).drop_max_connection = argument(0).boolean().unwrap_or(false);
                Ok(Value::Undefined)
            }
        }
    }

    fn command(&self, id: u32, command: Command) -> Result<(), Error> {
        let registry = self.registry();
        let registry = lock(&registry);
        let socket = registry.sockets.get(&id).ok_or_else(|| coded("ERR_STREAM_DESTROYED", "Cannot call write after a stream was destroyed"))?;
        socket.commands.send(command).map_err(|_| coded("ERR_STREAM_DESTROYED", "Cannot call write after a stream was destroyed"))
    }
}

// ---- Bindings ---------------------------------------------------------------------------------

impl Host for NodeView<'_> {
    async fn is_available(&mut self) -> wasmtime::Result<bool> {
        Ok(self.node.allows(Capability::Tls))
    }

    async fn start_tls(
        &mut self,
        server_name: String,
        input: Resource<DynInputStream>,
        output: Resource<DynOutputStream>,
    ) -> wasmtime::Result<Resource<HostFutureClientStreams>> {
        if let Err(error) = self.tls_allowed() {
            return Err(wasmtime::Error::msg(format!("{}: {}", error.code.unwrap_or_default(), error.message)));
        }
        // The same handshake `wasi:tls/types.client-handshake` performs, on the same context.
        let mut view = WasiTlsCtxView { table: &mut *self.table, ctx: &mut self.ctx.tls };
        let handshake = HostClientHandshake::new(&mut view, server_name, input, output)?;
        HostClientHandshake::finish(&mut view, handshake)
    }

    async fn query(&mut self, operation: QueryOperation, args: String) -> TlsResult<String> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let args = decode(&args).unwrap_or(Value::Array(Vec::new()));
        let value = match operation {
            // The suites the rustls provider offers, named as OpenSSL (and so Node) names them.
            QueryOperation::Ciphers => Ok(strings(
                [
                    "tls_aes_128_gcm_sha256",
                    "tls_aes_256_gcm_sha384",
                    "tls_chacha20_poly1305_sha256",
                    "ecdhe-ecdsa-aes128-gcm-sha256",
                    "ecdhe-ecdsa-aes256-gcm-sha384",
                    "ecdhe-ecdsa-chacha20-poly1305",
                    "ecdhe-rsa-aes128-gcm-sha256",
                    "ecdhe-rsa-aes256-gcm-sha384",
                    "ecdhe-rsa-chacha20-poly1305",
                ]
                .map(String::from),
            )),
            QueryOperation::CompressionAlgorithms => Ok(strings([])),
            QueryOperation::CaCertificates => self.certificates(args.at(0).and_then(Value::text).unwrap_or("default")),
            QueryOperation::CheckIdentity => {
                let hostname = args.at(0).and_then(Value::text).unwrap_or_default();
                Ok(certificate::check_identity(hostname, args.at(1).unwrap_or(&Value::Undefined)))
            }
        };
        Ok(value.map(|value| value.encode()))
    }

    async fn set_default_ca(&mut self, certs: String) -> TlsResult<()> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let Some(certs) = decode(&certs) else { return Ok(Err(bad_options())) };
        lock(&self.node.tls_contexts).default_ca = certs.materials();
        Ok(Ok(()))
    }

    async fn create_context(&mut self, options: String) -> TlsResult<u32> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let Some(options) = decode(&options) else { return Ok(Err(bad_options())) };
        let context = SecureContext::from_options(&options);
        let mut contexts = lock(&self.node.tls_contexts);
        contexts.next += 1;
        let id = contexts.next;
        contexts.contexts.insert(id, Arc::new(context));
        Ok(Ok(id))
    }

    async fn release_context(&mut self, id: u32) -> wasmtime::Result<()> {
        lock(&self.node.tls_contexts).contexts.remove(&id);
        Ok(())
    }

    async fn connect(&mut self, id: u32, options: String) -> TlsResult<()> {
        let connect = || -> Result<(), Error> {
            self.tls_allowed()?;
            let options = decode(&options).ok_or_else(bad_options)?;
            if options.get("path").and_then(Value::text).is_some() {
                return Err(unsupported("tls.connect({ path })", "it connects over TCP only"));
            }
            let registry = self.registry();
            if lock(&registry).sockets.contains_key(&id) {
                return Err(coded("ERR_INVALID_STATE", format!("TLS handle {id} is already in use")));
            }
            let context = self.context_for(&options)?;
            let host = options.get("host").and_then(Value::text).filter(|host| !host.is_empty()).unwrap_or("localhost").to_owned();
            let port = match options.get("port") {
                Some(Value::Number(port)) => *port,
                Some(Value::Text(port)) => port.parse().unwrap_or(-1.0),
                _ => -1.0,
            };
            if !(1.0..=65_535.0).contains(&port) || port.fract() != 0.0 {
                return Err(Error { name: "RangeError".into(), ..coded("ERR_SOCKET_BAD_PORT", "Port should be > 0 and < 65536.") });
            }
            let (connector, trust, name) = sockets::client(&context, &options, &host).map_err(to_error)?;
            let plan = Connect { host, port: port as u16, name, connector, trust, timeout: None };
            sockets::start_client(&registry, id, plan, context.own_certificate(), self.node.jobs.clone());
            // `timeout` is the socket's idle timeout, as `setTimeout` sets.
            if let Some(timeout) = options.get("timeout").and_then(Value::number).filter(|timeout| *timeout > 0.0) {
                self.command(id, Command::Timeout(timeout as u32))?;
            }
            Ok(())
        };
        Ok(connect())
    }

    async fn create_server(&mut self, id: u32, options: String) -> TlsResult<()> {
        let create = || -> Result<(), Error> {
            self.tls_allowed()?;
            let options = decode(&options).ok_or_else(bad_options)?;
            let context = self.context_for(&options)?;
            let setup = ServerSetup::new(context, &options).map_err(to_error)?;
            let registry = self.registry();
            let mut registry = lock(&registry);
            if registry.servers.contains_key(&id) {
                return Err(coded("ERR_INVALID_STATE", format!("TLS handle {id} is already in use")));
            }
            registry.servers.insert(id, ServerShared::new(Events::new(id, self.node.jobs.clone()), setup));
            Ok(())
        };
        Ok(create())
    }

    async fn socket_operation(&mut self, id: u32, operation: SocketOperationKind, args: String) -> TlsResult<String> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let args = decode(&args).unwrap_or(Value::Array(Vec::new()));
        Ok(NodeView::socket_operation(self, id, operation, args).await.map(|value| value.encode()))
    }

    async fn server_operation(&mut self, id: u32, operation: ServerOperationKind, args: String) -> TlsResult<String> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let args = decode(&args).unwrap_or(Value::Array(Vec::new()));
        Ok(NodeView::server_operation(self, id, operation, args).await.map(|value| value.encode()))
    }

    async fn write(&mut self, id: u32, token: u32, data: Vec<u8>) -> TlsResult<()> {
        Ok(self.tls_allowed().and_then(|()| self.command(id, Command::Write { token, data })))
    }

    async fn end(&mut self, id: u32, token: u32) -> TlsResult<()> {
        Ok(self.tls_allowed().and_then(|()| self.command(id, Command::End { token })))
    }

    async fn release(&mut self, id: u32) -> wasmtime::Result<()> {
        let registry = self.registry();
        let mut registry = lock(&registry);
        // Dropping a socket's handle ends its task and closes the connection.
        registry.sockets.remove(&id);
        if let Some(server) = registry.servers.remove(&id) {
            server.close(Value::Undefined);
        }
        Ok(())
    }
}
