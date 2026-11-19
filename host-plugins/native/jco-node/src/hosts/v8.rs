//! `jco:node/v8`.
//!
//! The serializer and deserializer speak V8's ValueSerializer wire format (version 15), byte for
//! byte as Node writes it, including `DefaultSerializer`'s host-object framing of `Buffer`s and
//! typed arrays, so data round-trips with `v8.serialize`/`deserialize` in a real Node. Values
//! cross the boundary as Jco's structured-value graphs (`{ root, nodes }`), with object identity
//! kept per writer and reader as the contract requires.
//!
//! What describes V8 itself has no V8 to describe here. The statistics report this host process's
//! memory in V8's field layout; profiles carry no samples; heap snapshots are refused with
//! `ERR_JCO_UNSUPPORTED_NODE_API`; flags and coverage control are accepted and have no effect.

use std::{collections::HashMap, time::SystemTime};

use serde_json::{Value as Json, json};
use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::v8::{Error, Host, HostProfile, HostReader, HostWriter, SnapshotOptions, StatisticsKind},
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

type V8Result<T> = wasmtime::Result<Result<T, Error>>;

const VERSION: u32 = 15;

/// Node's `arrayBufferViewTypes`, by the index `DefaultSerializer` writes; 10 is `FastBuffer`.
const VIEW_TYPES: [&str; 14] = [
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "Float32Array",
    "Float64Array",
    "DataView",
    "Buffer",
    "BigInt64Array",
    "BigUint64Array",
    "Float16Array",
];

/// V8's `ArrayBufferViewTag`s, for views serialized natively.
const VIEW_TAGS: [(&str, u8); 13] = [
    ("Int8Array", b'b'),
    ("Uint8Array", b'B'),
    ("Uint8ClampedArray", b'C'),
    ("Int16Array", b'w'),
    ("Uint16Array", b'W'),
    ("Int32Array", b'd'),
    ("Uint32Array", b'D'),
    ("Float16Array", b'h'),
    ("Float32Array", b'f'),
    ("Float64Array", b'F'),
    ("BigInt64Array", b'q'),
    ("BigUint64Array", b'Q'),
    ("DataView", b'?'),
];

fn error(message: impl Into<String>) -> Error {
    Error { name: "Error".into(), message: message.into(), code: None, errno: None, syscall: None }
}

fn coded(code: &str, message: impl Into<String>) -> Error {
    Error { code: Some(code.into()), ..error(message) }
}

fn clone_failed() -> Error {
    error("Unable to deserialize cloned data.")
}

fn unsupported(api: &str, detail: &str) -> Error {
    coded("ERR_JCO_UNSUPPORTED_NODE_API", format!("{api} is not supported in a WebAssembly component: {detail}"))
}

macro_rules! granted {
    ($view:expr) => {
        if !$view.node.allows(Capability::V8) {
            return Ok(Err(coded(&Capability::V8.denied_code(), denied_message(Capability::V8))));
        }
    };
}

// ---- Structured-value graphs ------------------------------------------------------------------

/// One node of Jco's structured-value graph (`internal/structured-value/types.ts`).
#[derive(Clone, Debug)]
enum Node {
    Undefined,
    Null,
    Bool(bool),
    Str(String),
    Number(f64),
    BigInt(String),
    Date(f64),
    Array { length: u32, entries: Vec<(String, usize)> },
    Object { entries: Vec<(String, usize)> },
    Map(Vec<(usize, usize)>),
    Set(Vec<usize>),
    Buffer(Vec<u8>),
    NodeBuffer(Vec<u8>),
    RegExp { source: String, flags: String },
    View { name: String, buffer: usize, offset: usize, length: usize },
}

fn bad_graph() -> Error {
    coded("ERR_INVALID_ARG_VALUE", "The structured value graph is malformed")
}

fn parse_number(text: &str) -> f64 {
    match text {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        text => text.parse().unwrap_or(f64::NAN),
    }
}

fn number_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        // Rust prints the shortest text that reads back as the same double, as JS does.
        format!("{value}")
    }
}

fn parse_node(node: &Json) -> Option<Node> {
    let entries = |key: &str| -> Option<Vec<(String, usize)>> {
        node.get(key)?
            .as_array()?
            .iter()
            .map(|entry| Some((entry.get(0)?.as_str()?.to_owned(), entry.get(1)?.as_u64()? as usize)))
            .collect()
    };
    let bytes = || -> Option<Vec<u8>> { node.get("bytes")?.as_array()?.iter().map(|byte| byte.as_u64().map(|byte| byte as u8)).collect() };
    let text = |key: &str| node.get(key).and_then(Json::as_str).map(str::to_owned);
    Some(match node.get("type")?.as_str()? {
        "undefined" => Node::Undefined,
        "value" => match node.get("value")? {
            Json::Null => Node::Null,
            Json::Bool(value) => Node::Bool(*value),
            Json::String(value) => Node::Str(value.clone()),
            _ => return None,
        },
        "number" => Node::Number(parse_number(&text("value")?)),
        "bigint" => Node::BigInt(text("value")?),
        "date" => Node::Date(parse_number(&text("value")?)),
        "array" => Node::Array { length: node.get("length")?.as_u64()? as u32, entries: entries("entries")? },
        "object" => Node::Object { entries: entries("entries")? },
        "map" => Node::Map(
            node.get("entries")?
                .as_array()?
                .iter()
                .map(|entry| Some((entry.get(0)?.as_u64()? as usize, entry.get(1)?.as_u64()? as usize)))
                .collect::<Option<_>>()?,
        ),
        "set" => Node::Set(node.get("entries")?.as_array()?.iter().map(|entry| entry.as_u64().map(|entry| entry as usize)).collect::<Option<_>>()?),
        "buffer" => Node::Buffer(bytes()?),
        "node-buffer" => Node::NodeBuffer(bytes()?),
        "regexp" => Node::RegExp { source: text("source")?, flags: text("flags")? },
        "view" => Node::View {
            name: text("name")?,
            buffer: node.get("buffer")?.as_u64()? as usize,
            offset: node.get("offset")?.as_u64()? as usize,
            length: node.get("length")?.as_u64()? as usize,
        },
        _ => return None,
    })
}

fn node_json(node: &Node) -> Json {
    let entries = |entries: &[(String, usize)]| Json::Array(entries.iter().map(|(key, value)| json!([key, value])).collect());
    match node {
        Node::Undefined => json!({ "type": "undefined" }),
        Node::Null => json!({ "type": "value", "value": null }),
        Node::Bool(value) => json!({ "type": "value", "value": value }),
        Node::Str(value) => json!({ "type": "value", "value": value }),
        Node::Number(value) => json!({ "type": "number", "value": number_text(*value) }),
        Node::BigInt(value) => json!({ "type": "bigint", "value": value }),
        Node::Date(value) => json!({ "type": "date", "value": number_text(*value) }),
        Node::Array { length, entries: items } => json!({ "type": "array", "length": length, "entries": entries(items) }),
        Node::Object { entries: items } => json!({ "type": "object", "entries": entries(items) }),
        Node::Map(items) => json!({ "type": "map", "entries": items.iter().map(|(key, value)| json!([key, value])).collect::<Vec<_>>() }),
        Node::Set(items) => json!({ "type": "set", "entries": items }),
        Node::Buffer(bytes) => json!({ "type": "buffer", "bytes": bytes }),
        Node::NodeBuffer(bytes) => json!({ "type": "node-buffer", "bytes": bytes }),
        Node::RegExp { source, flags } => json!({ "type": "regexp", "source": source, "flags": flags }),
        Node::View { name, buffer, offset, length } => {
            json!({ "type": "view", "name": name, "buffer": buffer, "offset": offset, "length": length })
        }
    }
}

fn is_object(node: &Node) -> bool {
    !matches!(node, Node::Undefined | Node::Null | Node::Bool(_) | Node::Str(_) | Node::Number(_) | Node::BigInt(_))
}

// ---- BigInt digits ----------------------------------------------------------------------------

/// A decimal BigInt as V8 stores it: sign and little-endian 64-bit digits.
fn bigint_digits(text: &str) -> Option<(bool, Vec<u64>)> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut words: Vec<u64> = Vec::new();
    for digit in digits.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        let mut carry = u128::from(digit - b'0');
        for word in &mut words {
            let product = u128::from(*word) * 10 + carry;
            *word = product as u64;
            carry = product >> 64;
        }
        if carry > 0 {
            words.push(carry as u64);
        }
    }
    Some((negative && !words.is_empty(), words))
}

fn bigint_text(negative: bool, mut words: Vec<u64>) -> String {
    while words.last() == Some(&0) {
        words.pop();
    }
    if words.is_empty() {
        return "0".into();
    }
    const CHUNK: u64 = 10_000_000_000_000_000_000;
    let mut parts = Vec::new();
    while !words.is_empty() {
        let mut remainder = 0u128;
        for word in words.iter_mut().rev() {
            let value = (remainder << 64) | u128::from(*word);
            *word = (value / u128::from(CHUNK)) as u64;
            remainder = value % u128::from(CHUNK);
        }
        while words.last() == Some(&0) {
            words.pop();
        }
        parts.push(remainder as u64);
    }
    let mut text = String::from(if negative { "-" } else { "" });
    text.push_str(&parts.pop().unwrap_or(0).to_string());
    for part in parts.iter().rev() {
        text.push_str(&format!("{part:019}"));
    }
    text
}

// ---- Writer -----------------------------------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn varint_length(mut value: u64) -> usize {
    let mut length = 1;
    while value >= 0x80 {
        value >>= 7;
        length += 1;
    }
    length
}

/// An array index key as V8 keeps it: a number rather than a string.
fn index_key(key: &str) -> Option<u32> {
    let index: u32 = key.parse().ok()?;
    (index.to_string() == key && index != u32::MAX).then_some(index)
}

pub struct Writer {
    defaults: bool,
    views_as_host_objects: bool,
    buffer: Vec<u8>,
    /// The values every `writeValue` graph so far has defined, as Jco's decode session keeps them.
    nodes: Vec<Node>,
    ids: HashMap<usize, u32>,
    next_id: u32,
}

impl Writer {
    fn new(defaults: bool) -> Self {
        Self { defaults, views_as_host_objects: defaults, buffer: Vec::new(), nodes: Vec::new(), ids: HashMap::new(), next_id: 0 }
    }

    fn tag(&mut self, tag: u8) {
        self.buffer.push(tag);
    }

    fn number(&mut self, value: f64) {
        // Small integers are Smis in V8 and go out as zigzag `I`; everything else as a double.
        if value.fract() == 0.0 && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) && !(value == 0.0 && value.is_sign_negative()) {
            self.tag(b'I');
            let value = value as i32;
            varint(&mut self.buffer, u64::from(((value << 1) ^ (value >> 31)) as u32));
        } else {
            self.tag(b'N');
            self.buffer.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn string(&mut self, value: &str) {
        if value.chars().all(|char| (char as u32) <= 0xFF) {
            self.tag(b'"');
            varint(&mut self.buffer, value.chars().count() as u64);
            self.buffer.extend(value.chars().map(|char| char as u8));
            return;
        }
        let units: Vec<u16> = value.encode_utf16().collect();
        let length = (units.len() * 2) as u64;
        // V8 keeps two-byte string data 2-aligned for its reader.
        if (self.buffer.len() + 1 + varint_length(length)) & 1 == 1 {
            self.tag(0);
        }
        self.tag(b'c');
        varint(&mut self.buffer, length);
        for unit in units {
            self.buffer.extend_from_slice(&unit.to_le_bytes());
        }
    }

    fn key(&mut self, key: &str) {
        match index_key(key) {
            Some(index) => self.number(f64::from(index)),
            None => self.string(key),
        }
    }

    fn node(&self, index: usize) -> Result<&Node, Error> {
        self.nodes.get(index).ok_or_else(bad_graph)
    }

    fn write(&mut self, index: usize) -> Result<(), Error> {
        let node = self.node(index)?.clone();
        if !is_object(&node) {
            match node {
                Node::Undefined => self.tag(b'_'),
                Node::Null => self.tag(b'0'),
                Node::Bool(true) => self.tag(b'T'),
                Node::Bool(false) => self.tag(b'F'),
                Node::Number(value) => self.number(value),
                Node::Str(value) => self.string(&value),
                Node::BigInt(text) => {
                    let (negative, words) = bigint_digits(&text).ok_or_else(bad_graph)?;
                    self.tag(b'Z');
                    varint(&mut self.buffer, (words.len() as u64 * 8) << 1 | u64::from(negative));
                    for word in words {
                        self.buffer.extend_from_slice(&word.to_le_bytes());
                    }
                }
                _ => unreachable!("objects are handled below"),
            }
            return Ok(());
        }
        if let Some(id) = self.ids.get(&index) {
            let id = *id;
            self.tag(b'^');
            varint(&mut self.buffer, u64::from(id));
            return Ok(());
        }
        let host = self.views_as_host_objects;
        // A natively serialized view goes out after its buffer, as V8 writes them.
        match &node {
            Node::View { buffer, .. } if !host => self.write(*buffer)?,
            Node::NodeBuffer(bytes) if !host => {
                // The view's own buffer: an object with an id of its own.
                self.next_id += 1;
                self.tag(b'B');
                varint(&mut self.buffer, bytes.len() as u64);
                self.buffer.extend_from_slice(bytes);
            }
            _ => {}
        }
        self.ids.insert(index, self.next_id);
        self.next_id += 1;
        match node {
            Node::Date(time) => {
                self.tag(b'D');
                self.buffer.extend_from_slice(&time.to_le_bytes());
            }
            Node::RegExp { source, flags } => {
                self.tag(b'R');
                self.string(&source);
                let bits = flags.chars().fold(0u64, |bits, flag| {
                    bits | match flag {
                        'g' => 1,
                        'i' => 2,
                        'm' => 4,
                        'y' => 8,
                        'u' => 16,
                        's' => 32,
                        'd' => 128,
                        'v' => 256,
                        _ => 0,
                    }
                });
                varint(&mut self.buffer, bits);
            }
            Node::Map(entries) => {
                self.tag(b';');
                for (key, value) in &entries {
                    self.write(*key)?;
                    self.write(*value)?;
                }
                self.tag(b':');
                varint(&mut self.buffer, entries.len() as u64 * 2);
            }
            Node::Set(entries) => {
                self.tag(b'\'');
                for value in &entries {
                    self.write(*value)?;
                }
                self.tag(b',');
                varint(&mut self.buffer, entries.len() as u64);
            }
            Node::Buffer(bytes) => {
                self.tag(b'B');
                varint(&mut self.buffer, bytes.len() as u64);
                self.buffer.extend_from_slice(&bytes);
            }
            Node::NodeBuffer(bytes) if host => self.host_object(10, &bytes),
            Node::NodeBuffer(bytes) => self.native_view(b'B', 0, bytes.len()),
            Node::View { name, buffer, offset, length } => {
                if host {
                    let Node::Buffer(bytes) = self.node(buffer)?.clone() else { return Err(bad_graph()) };
                    let data = bytes.get(offset..offset + length).ok_or_else(bad_graph)?;
                    let index = VIEW_TYPES.iter().position(|kind| *kind == name).ok_or_else(bad_graph)?;
                    if !self.defaults {
                        return Err(error(format!("Unserializable host object: [object {name}]")));
                    }
                    self.host_object(index as u32, data);
                } else {
                    let tag = VIEW_TAGS.iter().find(|(kind, _)| *kind == name).map(|(_, tag)| *tag).ok_or_else(bad_graph)?;
                    self.native_view(tag, offset, length);
                }
            }
            Node::Object { entries } => {
                self.tag(b'o');
                for (key, value) in &entries {
                    self.key(key);
                    self.write(*value)?;
                }
                self.tag(b'{');
                varint(&mut self.buffer, entries.len() as u64);
            }
            Node::Array { length, entries } => {
                let dense = (0..length).all(|position| entries.iter().any(|(key, _)| index_key(key) == Some(position)));
                if dense {
                    self.tag(b'A');
                    varint(&mut self.buffer, u64::from(length));
                    for position in 0..length {
                        let value = entries.iter().find(|(key, _)| index_key(key) == Some(position)).map(|(_, value)| *value);
                        self.write(value.ok_or_else(bad_graph)?)?;
                    }
                    let named: Vec<_> = entries.iter().filter(|(key, _)| index_key(key).is_none_or(|index| index >= length)).cloned().collect();
                    for (key, value) in &named {
                        self.key(key);
                        self.write(*value)?;
                    }
                    self.tag(b'$');
                    varint(&mut self.buffer, named.len() as u64);
                } else {
                    self.tag(b'a');
                    varint(&mut self.buffer, u64::from(length));
                    for (key, value) in &entries {
                        self.key(key);
                        self.write(*value)?;
                    }
                    self.tag(b'@');
                    varint(&mut self.buffer, entries.len() as u64);
                }
                varint(&mut self.buffer, u64::from(length));
            }
            _ => unreachable!("primitives are handled above"),
        }
        Ok(())
    }

    fn host_object(&mut self, index: u32, data: &[u8]) {
        self.tag(b'\\');
        varint(&mut self.buffer, u64::from(index));
        varint(&mut self.buffer, data.len() as u64);
        self.buffer.extend_from_slice(data);
    }

    fn native_view(&mut self, tag: u8, offset: usize, length: usize) {
        self.tag(b'V');
        self.tag(tag);
        varint(&mut self.buffer, offset as u64);
        varint(&mut self.buffer, length as u64);
        varint(&mut self.buffer, 0);
    }

    fn write_value(&mut self, graph: &str) -> Result<bool, Error> {
        let parsed: Json = serde_json::from_str(graph).map_err(|_| bad_graph())?;
        let (root, nodes) = match &parsed {
            Json::Array(nodes) => (0, nodes),
            Json::Object(object) => (
                object.get("root").and_then(Json::as_u64).ok_or_else(bad_graph)? as usize,
                object.get("nodes").and_then(Json::as_array).ok_or_else(bad_graph)?,
            ),
            _ => return Err(bad_graph()),
        };
        // A graph repeats the nodes earlier writes defined; only the new ones are added.
        for node in nodes.iter().skip(self.nodes.len()) {
            self.nodes.push(parse_node(node).ok_or_else(bad_graph)?);
        }
        let checkpoint = (self.buffer.len(), self.ids.clone(), self.next_id);
        if let Err(error) = self.write(root) {
            self.buffer.truncate(checkpoint.0);
            self.ids = checkpoint.1;
            self.next_id = checkpoint.2;
            return Err(error);
        }
        Ok(true)
    }
}

// ---- Reader -----------------------------------------------------------------------------------

pub struct Reader {
    defaults: bool,
    data: Vec<u8>,
    at: usize,
    version: u32,
    /// Every value read so far, as the persistent codec Jco's provider encodes them with.
    nodes: Vec<Node>,
    ids: HashMap<u32, usize>,
    next_id: u32,
}

impl Reader {
    fn byte(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn peek_tag(&mut self) -> Option<u8> {
        while self.data.get(self.at) == Some(&0) {
            self.at += 1;
        }
        self.data.get(self.at).copied()
    }

    fn tag(&mut self) -> Option<u8> {
        let tag = self.peek_tag()?;
        self.at += 1;
        Some(tag)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn bytes(&mut self, length: usize) -> Option<Vec<u8>> {
        let end = self.at.checked_add(length)?;
        let bytes = self.data.get(self.at..end)?.to_vec();
        self.at = end;
        Some(bytes)
    }

    fn double(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }

    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Reserve a node for an object before reading what it contains, as V8 assigns its id.
    fn reserve(&mut self) -> usize {
        let index = self.push(Node::Undefined);
        self.ids.insert(self.next_id, index);
        self.next_id += 1;
        index
    }

    fn string_body(&mut self, tag: u8) -> Option<String> {
        let length = self.varint()? as usize;
        let bytes = self.bytes(length)?;
        Some(match tag {
            b'S' => String::from_utf8_lossy(&bytes).into_owned(),
            b'"' => bytes.iter().map(|byte| char::from(*byte)).collect(),
            b'c' => {
                let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
                String::from_utf16_lossy(&units)
            }
            _ => return None,
        })
    }

    fn key(&mut self) -> Option<String> {
        let index = self.value()?;
        match &self.nodes[index] {
            Node::Str(key) => Some(key.clone()),
            Node::Number(key) => Some(number_text(*key)),
            _ => None,
        }
    }

    /// Key/value pairs up to `end`, then the property count V8 writes after it.
    fn properties(&mut self, end: u8) -> Option<Vec<(String, usize)>> {
        let mut entries = Vec::new();
        loop {
            if self.peek_tag()? == end {
                self.at += 1;
                self.varint()?;
                return Some(entries);
            }
            let key = self.key()?;
            let value = self.value()?;
            entries.push((key, value));
        }
    }

    fn view(&mut self, buffer: usize) -> Option<usize> {
        let tag = self.byte()?;
        let offset = self.varint()? as usize;
        let length = self.varint()? as usize;
        if self.version >= 14 {
            self.varint()?;
        }
        let name = VIEW_TAGS.iter().find(|(_, kind)| *kind == tag)?.0;
        let index = self.reserve();
        self.nodes[index] = Node::View { name: name.into(), buffer, offset, length };
        Some(index)
    }

    fn value(&mut self) -> Option<usize> {
        let tag = self.tag()?;
        let simple = match tag {
            b'_' => Some(Node::Undefined),
            b'0' => Some(Node::Null),
            b'T' => Some(Node::Bool(true)),
            b'F' => Some(Node::Bool(false)),
            b'I' => {
                let raw = self.varint()? as u32;
                Some(Node::Number(f64::from(((raw >> 1) as i32) ^ -((raw & 1) as i32))))
            }
            b'U' => Some(Node::Number(self.varint()? as f64)),
            b'N' => Some(Node::Number(self.double()?)),
            b'Z' => {
                let bitfield = self.varint()?;
                let bytes = self.bytes((bitfield >> 1) as usize)?;
                let words = bytes.chunks(8).map(|chunk| {
                    let mut word = [0u8; 8];
                    word[..chunk.len()].copy_from_slice(chunk);
                    u64::from_le_bytes(word)
                });
                Some(Node::BigInt(bigint_text(bitfield & 1 == 1, words.collect())))
            }
            b'S' | b'"' | b'c' => Some(Node::Str(self.string_body(tag)?)),
            _ => None,
        };
        if let Some(node) = simple {
            return Some(self.push(node));
        }
        match tag {
            b'^' => {
                let id = self.varint()? as u32;
                let index = *self.ids.get(&id)?;
                if matches!(self.nodes[index], Node::Buffer(_)) && self.peek_tag() == Some(b'V') {
                    self.at += 1;
                    return self.view(index);
                }
                Some(index)
            }
            b'o' => {
                let index = self.reserve();
                let entries = self.properties(b'{')?;
                self.nodes[index] = Node::Object { entries };
                Some(index)
            }
            b'A' => {
                let index = self.reserve();
                let length = self.varint()? as u32;
                let mut entries = Vec::new();
                for position in 0..length {
                    if self.peek_tag()? == b'-' {
                        self.at += 1;
                        continue;
                    }
                    let value = self.value()?;
                    entries.push((position.to_string(), value));
                }
                entries.extend(self.properties(b'$')?);
                self.varint()?;
                self.nodes[index] = Node::Array { length, entries };
                Some(index)
            }
            b'a' => {
                let index = self.reserve();
                let length = self.varint()? as u32;
                let entries = self.properties(b'@')?;
                self.varint()?;
                self.nodes[index] = Node::Array { length, entries };
                Some(index)
            }
            b'D' => {
                let index = self.reserve();
                self.nodes[index] = Node::Date(self.double()?);
                Some(index)
            }
            b'R' => {
                let index = self.reserve();
                let tag = self.tag()?;
                let source = self.string_body(tag)?;
                let bits = self.varint()?;
                let flags = [(128, 'd'), (1, 'g'), (2, 'i'), (4, 'm'), (32, 's'), (16, 'u'), (256, 'v'), (8, 'y')]
                    .iter()
                    .filter(|(bit, _)| bits & bit != 0)
                    .map(|(_, flag)| *flag)
                    .collect();
                self.nodes[index] = Node::RegExp { source, flags };
                Some(index)
            }
            b';' => {
                let index = self.reserve();
                let mut entries = Vec::new();
                while self.peek_tag()? != b':' {
                    let key = self.value()?;
                    let value = self.value()?;
                    entries.push((key, value));
                }
                self.at += 1;
                self.varint()?;
                self.nodes[index] = Node::Map(entries);
                Some(index)
            }
            b'\'' => {
                let index = self.reserve();
                let mut entries = Vec::new();
                while self.peek_tag()? != b',' {
                    entries.push(self.value()?);
                }
                self.at += 1;
                self.varint()?;
                self.nodes[index] = Node::Set(entries);
                Some(index)
            }
            b'B' => {
                let index = self.reserve();
                let length = self.varint()? as usize;
                self.nodes[index] = Node::Buffer(self.bytes(length)?);
                if self.peek_tag() == Some(b'V') {
                    self.at += 1;
                    return self.view(index);
                }
                Some(index)
            }
            b'\\' if self.defaults => {
                // `DefaultDeserializer._readHostObject`: a copy of the view's bytes.
                let index = self.reserve();
                let kind = self.varint()? as usize;
                let length = self.varint()? as usize;
                let bytes = self.bytes(length)?;
                let name = *VIEW_TYPES.get(kind)?;
                self.nodes[index] = if name == "Buffer" {
                    Node::NodeBuffer(bytes)
                } else {
                    let buffer = self.push(Node::Buffer(bytes));
                    Node::View { name: name.into(), buffer, offset: 0, length }
                };
                Some(index)
            }
            _ => None,
        }
    }

    fn read_value(&mut self) -> Result<String, Error> {
        let start = (self.at, self.nodes.len(), self.ids.clone(), self.next_id);
        let Some(root) = self.value() else {
            self.at = start.0;
            self.nodes.truncate(start.1);
            self.ids = start.2;
            self.next_id = start.3;
            return Err(clone_failed());
        };
        let nodes: Vec<Json> = self.nodes.iter().map(node_json).collect();
        Ok(json!({ "root": root, "nodes": nodes }).to_string())
    }
}

// ---- Statistics -------------------------------------------------------------------------------

fn process_memory() -> (u64, u64) {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::get_current_pid().ok();
    if let Some(pid) = pid {
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    }
    pid.and_then(|pid| system.process(pid)).map_or((1, 1), |process| (process.memory().max(1), process.virtual_memory().max(1)))
}

fn statistics(kind: StatisticsKind) -> Json {
    let (resident, total) = process_memory();
    match kind {
        StatisticsKind::Heap => json!({
            "total_heap_size": resident,
            "total_heap_size_executable": 0,
            "total_physical_size": resident,
            "total_available_size": total.saturating_sub(resident),
            "used_heap_size": resident,
            "heap_size_limit": total,
            "malloced_memory": 0,
            "peak_malloced_memory": 0,
            "does_zap_garbage": 0,
            "number_of_native_contexts": 1,
            "number_of_detached_contexts": 0,
            "total_global_handles_size": 0,
            "used_global_handles_size": 0,
            "external_memory": 0,
        }),
        StatisticsKind::Spaces => json!([{
            "space_name": "old_space",
            "space_size": resident,
            "space_used_size": resident,
            "space_available_size": total.saturating_sub(resident),
            "physical_space_size": resident,
        }]),
        StatisticsKind::Code => json!({
            "code_and_metadata_size": resident,
            "bytecode_and_metadata_size": 0,
            "external_script_source_size": 0,
            "cpu_profiler_metadata_size": 0,
        }),
        StatisticsKind::CppBrief | StatisticsKind::CppDetailed => json!({
            "committed_size_bytes": 0,
            "resident_size_bytes": 0,
            "used_size_bytes": 0,
            "space_statistics": [],
            "type_names": [],
            "detail_level": if matches!(kind, StatisticsKind::CppBrief) { "brief" } else { "detailed" },
        }),
    }
}

fn now_micros() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_micros() as u64)
}

pub struct Profile {
    cpu: bool,
    started: u64,
    stopped: bool,
}

// ---- Host -------------------------------------------------------------------------------------

impl Host for NodeView<'_> {
    async fn cached_data_version_tag(&mut self) -> V8Result<u32> {
        granted!(self);
        // Stable for this build, as V8's tag is for a V8 version and flag set.
        let tag = env!("CARGO_PKG_VERSION").bytes().fold(0x811C_9DC5u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193));
        Ok(Ok(tag.max(1)))
    }

    async fn statistics(&mut self, kind: StatisticsKind) -> V8Result<String> {
        granted!(self);
        Ok(Ok(statistics(kind).to_string()))
    }

    async fn get_heap_snapshot(&mut self, _options: SnapshotOptions) -> V8Result<Vec<u8>> {
        granted!(self);
        Ok(Err(unsupported("v8.getHeapSnapshot()", "the host has no V8 heap to snapshot")))
    }

    async fn write_heap_snapshot(&mut self, _filename: Option<String>, _options: SnapshotOptions) -> V8Result<String> {
        granted!(self);
        Ok(Err(unsupported("v8.writeHeapSnapshot()", "the host has no V8 heap to snapshot")))
    }

    async fn set_flags_from_string(&mut self, _flags: String) -> V8Result<()> {
        granted!(self);
        Ok(Ok(()))
    }

    async fn take_coverage(&mut self) -> V8Result<()> {
        granted!(self);
        Ok(Ok(()))
    }

    async fn stop_coverage(&mut self) -> V8Result<()> {
        granted!(self);
        Ok(Ok(()))
    }

    async fn set_heap_snapshot_near_heap_limit(&mut self, _limit: u32) -> V8Result<()> {
        granted!(self);
        Ok(Ok(()))
    }

    async fn open_writer(&mut self, defaults: bool) -> V8Result<Resource<Writer>> {
        granted!(self);
        Ok(Ok(self.table.push(Writer::new(defaults))?))
    }

    async fn open_reader(&mut self, data: Vec<u8>, defaults: bool) -> V8Result<Resource<Reader>> {
        granted!(self);
        let reader = Reader { defaults, data, at: 0, version: 0, nodes: Vec::new(), ids: HashMap::new(), next_id: 0 };
        Ok(Ok(self.table.push(reader)?))
    }

    async fn start_profile(&mut self, cpu: bool) -> V8Result<Resource<Profile>> {
        granted!(self);
        Ok(Ok(self.table.push(Profile { cpu, started: now_micros(), stopped: false })?))
    }

    async fn release_writer(&mut self, handle: Resource<Writer>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }

    async fn release_reader(&mut self, handle: Resource<Reader>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }

    async fn release_profile(&mut self, handle: Resource<Profile>) -> wasmtime::Result<()> {
        self.table.delete(handle)?;
        Ok(())
    }
}

impl HostWriter for NodeView<'_> {
    async fn write_header(&mut self, self_: Resource<Writer>) -> V8Result<()> {
        let writer = self.table.get_mut(&self_)?;
        writer.buffer.push(0xFF);
        varint(&mut writer.buffer, u64::from(VERSION));
        Ok(Ok(()))
    }

    async fn write_value(&mut self, self_: Resource<Writer>, graph: String) -> V8Result<bool> {
        Ok(self.table.get_mut(&self_)?.write_value(&graph))
    }

    async fn release_buffer(&mut self, self_: Resource<Writer>) -> V8Result<Vec<u8>> {
        Ok(Ok(std::mem::take(&mut self.table.get_mut(&self_)?.buffer)))
    }

    async fn write_uint32(&mut self, self_: Resource<Writer>, value: f64) -> V8Result<()> {
        varint(&mut self.table.get_mut(&self_)?.buffer, u64::from(to_uint32(value)));
        Ok(Ok(()))
    }

    async fn write_uint64(&mut self, self_: Resource<Writer>, hi: f64, lo: f64) -> V8Result<()> {
        let value = (u64::from(to_uint32(hi)) << 32) | u64::from(to_uint32(lo));
        varint(&mut self.table.get_mut(&self_)?.buffer, value);
        Ok(Ok(()))
    }

    async fn write_double(&mut self, self_: Resource<Writer>, value: f64) -> V8Result<()> {
        self.table.get_mut(&self_)?.buffer.extend_from_slice(&value.to_le_bytes());
        Ok(Ok(()))
    }

    async fn write_raw_bytes(&mut self, self_: Resource<Writer>, data: Vec<u8>) -> V8Result<()> {
        self.table.get_mut(&self_)?.buffer.extend_from_slice(&data);
        Ok(Ok(()))
    }

    async fn set_treat_views_as_host_objects(&mut self, self_: Resource<Writer>, flag: bool) -> V8Result<()> {
        self.table.get_mut(&self_)?.views_as_host_objects = flag;
        Ok(Ok(()))
    }

    async fn drop(&mut self, rep: Resource<Writer>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

/// ECMAScript `ToUint32`.
fn to_uint32(value: f64) -> u32 {
    if !value.is_finite() {
        return 0;
    }
    value.trunc().rem_euclid(4_294_967_296.0) as u32
}

impl HostReader for NodeView<'_> {
    async fn read_header(&mut self, self_: Resource<Reader>) -> V8Result<bool> {
        let reader = self.table.get_mut(&self_)?;
        if reader.data.get(reader.at) == Some(&0xFF) {
            reader.at += 1;
            match reader.varint() {
                Some(version) if version <= u64::from(VERSION) => reader.version = version as u32,
                _ => return Ok(Err(error("Unable to deserialize cloned data due to invalid or unsupported version."))),
            }
        }
        Ok(Ok(true))
    }

    async fn read_value(&mut self, self_: Resource<Reader>) -> V8Result<String> {
        Ok(self.table.get_mut(&self_)?.read_value())
    }

    async fn get_wire_format_version(&mut self, self_: Resource<Reader>) -> V8Result<u32> {
        Ok(Ok(self.table.get(&self_)?.version))
    }

    async fn read_uint32(&mut self, self_: Resource<Reader>) -> V8Result<u32> {
        let reader = self.table.get_mut(&self_)?;
        Ok(reader.varint().and_then(|value| u32::try_from(value).ok()).ok_or_else(|| error("ReadUint32() failed")))
    }

    async fn read_uint64(&mut self, self_: Resource<Reader>) -> V8Result<(u32, u32)> {
        let reader = self.table.get_mut(&self_)?;
        Ok(reader.varint().map(|value| ((value >> 32) as u32, value as u32)).ok_or_else(|| error("ReadUint64() failed")))
    }

    async fn read_double(&mut self, self_: Resource<Reader>) -> V8Result<f64> {
        Ok(self.table.get_mut(&self_)?.double().ok_or_else(|| error("ReadDouble() failed")))
    }

    async fn read_raw_bytes(&mut self, self_: Resource<Reader>, length: f64) -> V8Result<Vec<u8>> {
        let reader = self.table.get_mut(&self_)?;
        let bytes = (length >= 0.0 && length.fract() == 0.0).then(|| reader.bytes(length as usize)).flatten();
        Ok(bytes.ok_or_else(|| error("ReadRawBytes() failed")))
    }

    async fn drop(&mut self, rep: Resource<Reader>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostProfile for NodeView<'_> {
    async fn stop(&mut self, self_: Resource<Profile>) -> V8Result<Option<String>> {
        let profile = self.table.get_mut(&self_)?;
        if profile.stopped {
            return Ok(Ok(None));
        }
        profile.stopped = true;
        let end = now_micros();
        // A host-side profile has no guest frames to sample: a root and no samples.
        let data = if profile.cpu {
            json!({
                "nodes": [{
                    "id": 1,
                    "callFrame": { "functionName": "(root)", "scriptId": "0", "url": "", "lineNumber": -1, "columnNumber": -1 },
                    "hitCount": 0,
                    "children": [],
                }],
                "startTime": profile.started,
                "endTime": end,
                "samples": [],
                "timeDeltas": [],
            })
        } else {
            json!({ "version": 1, "startTime": profile.started / 1000, "endTime": end / 1000, "statistics": [] })
        };
        Ok(Ok(Some(data.to_string())))
    }

    async fn drop(&mut self, rep: Resource<Profile>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn serialize(graph: Json) -> String {
        let mut writer = Writer::new(true);
        writer.buffer.extend_from_slice(&[0xFF, 0x0F]);
        writer.write_value(&graph.to_string()).unwrap();
        hex(&writer.buffer)
    }

    // Expected bytes are what Node's `v8.serialize` produces for the same values.
    #[test]
    fn writes_what_node_writes() {
        assert_eq!(serialize(json!([{ "type": "node-buffer", "bytes": [97, 98] }])), "ff0f5c0a026162");
        assert_eq!(
            serialize(json!([
                { "type": "object", "entries": [["0", 1], ["a", 2], ["b", 3], ["c", 4]] },
                { "type": "number", "value": "2" },
                { "type": "number", "value": "1" },
                { "type": "number", "value": "-0" },
                { "type": "number", "value": "2147483648" },
            ])),
            "ff0f6f4900490422016149022201624e00000000000000802201634e000000000000e0417b04"
        );
        assert_eq!(
            serialize(json!([
                { "type": "array", "length": 3, "entries": [["0", 1], ["1", 2], ["2", 3]] },
                { "type": "number", "value": "1" },
                { "type": "value", "value": "é" },
                { "type": "value", "value": "💚" },
            ])),
            "ff0f410349022201e90063043dd89adc240003"
        );
        assert_eq!(serialize(json!([{ "type": "bigint", "value": "123" }])), "ff0f5a107b00000000000000");
        assert_eq!(serialize(json!([{ "type": "bigint", "value": "-18446744073709551616" }])), "ff0f5a2100000000000000000100000000000000");
    }

    #[test]
    fn reads_back_references_and_views() {
        // [m, m, new Map([[m, m]]), new Set([m]), /a/gi, new Date(5)] from Node.
        let bytes = "ff0f41066f22017849027b015e013b5e015e013a02275e012c015222016103440000000000001440240006";
        let data = (0..bytes.len()).step_by(2).map(|at| u8::from_str_radix(&bytes[at..at + 2], 16).unwrap()).collect();
        let mut reader = Reader { defaults: true, data, at: 2, version: 15, nodes: Vec::new(), ids: HashMap::new(), next_id: 0 };
        let graph: Json = serde_json::from_str(&reader.read_value().unwrap()).unwrap();
        let nodes = graph["nodes"].as_array().unwrap();
        let root = &nodes[graph["root"].as_u64().unwrap() as usize];
        let entries = root["entries"].as_array().unwrap();
        assert_eq!(entries[0][1], entries[1][1]);
        assert_eq!(nodes[entries[4][1].as_u64().unwrap() as usize]["flags"], "gi");
        assert_eq!(nodes[entries[5][1].as_u64().unwrap() as usize]["value"], "5");
    }

    #[test]
    fn bigints_round_trip() {
        for text in ["0", "123", "-18446744073709551616", "340282366920938463463374607431768211455"] {
            let (negative, words) = bigint_digits(text).unwrap();
            assert_eq!(bigint_text(negative, words), text);
        }
    }
}
