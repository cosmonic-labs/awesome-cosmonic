//! `jco:node/tls`: TLS for the guest's `node:tls`, `node:https` and secure `node:http2`.
//!
//! Two parts of the contract are provided:
//!
//! - `start-tls` upgrades a pair of `wasi:io` streams to a `wasi:tls` client connection, using
//!   the same rustls-backed `wasi:tls@0.2.0-draft` implementation this plugin links.
//! - Secure contexts (`create-context`), which this host's HTTP/2 servers and clients use.
//!
//! The rest is Jco's emulation of Node's `tls` sockets and servers on the host (`connect`,
//! `create-server` and their operations, which its Node provider runs on a worker thread). It is
//! not provided: those calls fail as unsupported Node APIs.

use std::{collections::HashMap, sync::Arc};

use wash_runtime::wasmtime::{self, component::Resource};
use wasmtime_wasi::p2::{DynInputStream, DynOutputStream};
use wasmtime_wasi_tls::{
    WasiTlsCtxView,
    p2::{HostFutureClientStreams, bindings::types::HostClientHandshake},
};

use crate::{
    bindings::jco::node::tls::{Error, Host, QueryOperation, ServerOperationKind, SocketOperationKind},
    hosts::denied_message,
    plugin::NodeView,
    state::{Capability, lock},
};

type TlsResult<T> = wasmtime::Result<Result<T, Error>>;

fn coded(code: &str, message: impl Into<String>) -> Error {
    Error { name: "Error".into(), message: message.into(), code: Some(code.into()) }
}

fn unsupported(what: &str) -> Error {
    coded(
        "ERR_JCO_UNSUPPORTED_NODE_API",
        format!("{what} is not supported by this host: it provides TLS through wasi:tls and HTTP/2, not Node TLS sockets"),
    )
}

/// Key material and trust settings from a guest's `tls.createSecureContext` options.
#[derive(Clone, Default)]
pub(crate) struct SecureContext {
    pub(crate) key: Option<Vec<u8>>,
    pub(crate) cert: Option<Vec<u8>>,
    pub(crate) ca: Vec<Vec<u8>>,
    pub(crate) reject_unauthorized: Option<bool>,
}

#[derive(Default)]
pub(crate) struct Contexts {
    next: u32,
    contexts: HashMap<u32, Arc<SecureContext>>,
    pub(crate) default_ca: Option<Vec<u8>>,
}

impl NodeView<'_> {
    /// The secure context `id`, for the HTTP/2 host.
    pub(crate) fn secure_context(&self, id: u32) -> Option<Arc<SecureContext>> {
        lock(&self.node.tls_contexts).contexts.get(&id).cloned()
    }
}

// ---- The wire graph ---------------------------------------------------------------------------

/// A value decoded from Jco's TLS wire graph (`tls/wire.ts`): JSON with shared and binary nodes.
/// Every kind the format carries is decoded, though contexts read only some of them.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

fn decode(text: &str) -> Option<Value> {
    let graph: serde_json::Value = serde_json::from_str(text).ok()?;
    let nodes = graph.get("nodes")?.as_array()?;
    fn visit(value: &serde_json::Value, nodes: &[serde_json::Value], depth: usize) -> Value {
        if depth > 64 {
            return Value::Null;
        }
        match value {
            serde_json::Value::Bool(value) => Value::Bool(*value),
            serde_json::Value::Number(value) => Value::Number(value.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(value) => Value::Text(value.clone()),
            serde_json::Value::Object(object) => {
                if let Some(index) = object.get("ref").and_then(serde_json::Value::as_u64) {
                    let Some(node) = nodes.get(index as usize) else { return Value::Null };
                    let data = node.get("data");
                    return match node.get("kind").and_then(serde_json::Value::as_str) {
                        Some("bytes") => Value::Bytes(
                            data.and_then(serde_json::Value::as_array)
                                .map(|bytes| bytes.iter().map(|byte| byte.as_u64().unwrap_or(0) as u8).collect())
                                .unwrap_or_default(),
                        ),
                        Some("array") => Value::Array(
                            data.and_then(serde_json::Value::as_array)
                                .map(|items| items.iter().map(|item| visit(item, nodes, depth + 1)).collect())
                                .unwrap_or_default(),
                        ),
                        _ => Value::Object(
                            data.and_then(serde_json::Value::as_array)
                                .map(|entries| {
                                    entries
                                        .iter()
                                        .filter_map(|entry| {
                                            let entry = entry.as_array()?;
                                            Some((entry.first()?.as_str()?.to_owned(), visit(entry.get(1)?, nodes, depth + 1)))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                        ),
                    };
                }
                if let Some(number) = object.get("number").and_then(serde_json::Value::as_str) {
                    return Value::Number(number.parse().unwrap_or(f64::NAN));
                }
                Value::Null
            }
            _ => Value::Null,
        }
    }
    Some(visit(graph.get("root")?, nodes, 0))
}

fn encode_strings(items: &[String]) -> String {
    serde_json::json!({ "root": { "ref": 0 }, "nodes": [{ "kind": "array", "data": items }] }).to_string()
}

impl Value {
    fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    /// PEM or DER material, given as a string or bytes (or the first of an array of them).
    fn material(&self) -> Option<Vec<u8>> {
        match self {
            Value::Text(text) => Some(text.as_bytes().to_vec()),
            Value::Bytes(bytes) => Some(bytes.clone()),
            Value::Array(items) => items.first().and_then(Value::material),
            _ => None,
        }
    }

    fn materials(&self) -> Vec<Vec<u8>> {
        match self {
            Value::Array(items) => items.iter().filter_map(Value::material).collect(),
            other => other.material().into_iter().collect(),
        }
    }
}

// ---- Bindings ---------------------------------------------------------------------------------

impl NodeView<'_> {
    fn tls_allowed(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Tls) {
            Ok(())
        } else {
            Err(coded(&Capability::Tls.denied_code(), denied_message(Capability::Tls)))
        }
    }
}

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

    async fn query(&mut self, operation: QueryOperation, _args: String) -> TlsResult<String> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        Ok(match operation {
            // The suites the rustls provider offers, named as OpenSSL (and so Node) names them.
            QueryOperation::Ciphers => Ok(encode_strings(
                &[
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
            QueryOperation::CompressionAlgorithms => Ok(encode_strings(&[])),
            QueryOperation::CaCertificates => Err(unsupported("tls.getCACertificates()")),
            QueryOperation::CheckIdentity => Err(unsupported("tls.checkServerIdentity()")),
        })
    }

    async fn set_default_ca(&mut self, certs: String) -> TlsResult<()> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let ca = decode(&certs).map(|value| value.materials().concat());
        lock(&self.node.tls_contexts).default_ca = ca;
        Ok(Ok(()))
    }

    async fn create_context(&mut self, options: String) -> TlsResult<u32> {
        if let Err(error) = self.tls_allowed() {
            return Ok(Err(error));
        }
        let Some(options) = decode(&options) else {
            return Ok(Err(coded("ERR_INVALID_ARG_VALUE", "the secure context options could not be decoded")));
        };
        let context = SecureContext {
            key: options.get("key").and_then(Value::material),
            cert: options.get("cert").and_then(Value::material),
            ca: options.get("ca").map(Value::materials).unwrap_or_default(),
            reject_unauthorized: match options.get("rejectUnauthorized") {
                Some(Value::Bool(value)) => Some(*value),
                _ => None,
            },
        };
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

    async fn connect(&mut self, _id: u32, _options: String) -> TlsResult<()> {
        Ok(self.tls_allowed().and(Err(unsupported("tls.connect()"))))
    }

    async fn create_server(&mut self, _id: u32, _options: String) -> TlsResult<()> {
        Ok(self.tls_allowed().and(Err(unsupported("tls.createServer()"))))
    }

    async fn socket_operation(&mut self, _id: u32, _operation: SocketOperationKind, _args: String) -> TlsResult<String> {
        Ok(self.tls_allowed().and(Err(unsupported("a tls.TLSSocket operation"))))
    }

    async fn server_operation(&mut self, _id: u32, _operation: ServerOperationKind, _args: String) -> TlsResult<String> {
        Ok(self.tls_allowed().and(Err(unsupported("a tls.Server operation"))))
    }

    async fn write(&mut self, _id: u32, _token: u32, _data: Vec<u8>) -> TlsResult<()> {
        Ok(self.tls_allowed().and(Err(unsupported("tls.TLSSocket.write()"))))
    }

    async fn end(&mut self, _id: u32, _token: u32) -> TlsResult<()> {
        Ok(self.tls_allowed().and(Err(unsupported("tls.TLSSocket.end()"))))
    }

    async fn release(&mut self, _id: u32) -> wasmtime::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_decode_from_the_wire_graph() {
        // encode({ key: Uint8Array.of(1, 2), cert: "PEM", ca: ["A", Uint8Array.of(3)], rejectUnauthorized: false })
        let text = r#"{"root":{"ref":0},"nodes":[{"kind":"object","data":[["key",{"ref":1}],["cert","PEM"],["ca",{"ref":2}],["rejectUnauthorized",false]]},{"kind":"bytes","data":[1,2]},{"kind":"array","data":["A",{"ref":3}]},{"kind":"bytes","data":[3]}]}"#;
        let value = decode(text).unwrap();
        assert_eq!(value.get("key").and_then(Value::material), Some(vec![1, 2]));
        assert_eq!(value.get("cert").and_then(Value::material), Some(b"PEM".to_vec()));
        assert_eq!(value.get("ca").map(Value::materials), Some(vec![b"A".to_vec(), vec![3]]));
        assert!(matches!(value.get("rejectUnauthorized"), Some(Value::Bool(false))));
    }
}
