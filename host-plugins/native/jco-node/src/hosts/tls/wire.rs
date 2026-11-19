//! Jco's TLS wire graph (`tls/wire.ts`): JSON whose objects, arrays and byte strings are nodes
//! referenced by index, so values keep binary fields, error names and shared (even cyclic) parts.

use serde_json::json;

/// A value as the wire graph carries it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
    /// An `Error` (named, for `TypeError` and the like) with its own properties.
    Error(String, Vec<(String, Value)>),
    /// The object this value is a property of: a self-signed certificate's `issuerCertificate`.
    /// Only encoded; decoding cuts cycles to `Null` instead.
    Parent,
}

/// An object from `(key, value)` pairs, leaving out the `Undefined` ones as Node's would lack them.
pub(crate) fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries.into_iter().filter(|(_, value)| *value != Value::Undefined).map(|(key, value)| (key.to_owned(), value)).collect(),
    )
}

/// An `Error` with `message`, and `code` if there is one.
pub(crate) fn error(code: Option<&str>, message: impl Into<String>) -> Value {
    error_with(code, message, [])
}

pub(crate) fn error_with<const N: usize>(code: Option<&str>, message: impl Into<String>, extra: [(&str, Value); N]) -> Value {
    let mut entries = vec![("message".to_owned(), Value::Text(message.into()))];
    if let Some(code) = code {
        entries.push(("code".to_owned(), Value::Text(code.to_owned())));
    }
    entries.extend(extra.into_iter().filter(|(_, value)| *value != Value::Undefined).map(|(key, value)| (key.to_owned(), value)));
    Value::Error("Error".into(), entries)
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Text(value.to_owned())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Text(value)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Value::Undefined, Into::into)
    }
}

macro_rules! number {
    ($($kind:ty),*) => {$(
        impl From<$kind> for Value {
            fn from(value: $kind) -> Self {
                Value::Number(value as f64)
            }
        }
    )*};
}
number!(u16, u32, u64, usize, i32, f64);

impl Value {
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) | Value::Error(_, entries) => {
                entries.iter().find(|(name, _)| name == key).map(|(_, value)| value)
            }
            _ => None,
        }
    }

    /// The `index`th element of an array (an operation's arguments).
    pub(crate) fn at(&self, index: usize) -> Option<&Value> {
        match self {
            Value::Array(items) => items.get(index),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn number(&self) -> Option<f64> {
        match self {
            Value::Number(number) => Some(*number),
            _ => None,
        }
    }

    pub(crate) fn boolean(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// PEM or DER material, given as a string or bytes (or the first of an array of them).
    pub(crate) fn material(&self) -> Option<Vec<u8>> {
        match self {
            Value::Text(text) => Some(text.as_bytes().to_vec()),
            Value::Bytes(bytes) => Some(bytes.clone()),
            Value::Array(items) => items.first().and_then(Value::material),
            _ => None,
        }
    }

    pub(crate) fn materials(&self) -> Vec<Vec<u8>> {
        match self {
            Value::Array(items) => items.iter().filter_map(Value::material).collect(),
            other => other.material().into_iter().collect(),
        }
    }

    /// The graph text for this value.
    pub(crate) fn encode(&self) -> String {
        let mut nodes = Vec::new();
        let root = encode_value(self, &mut nodes, &mut Vec::new());
        json!({ "root": root, "nodes": nodes }).to_string()
    }
}

fn encode_value(value: &Value, nodes: &mut Vec<serde_json::Value>, parents: &mut Vec<usize>) -> serde_json::Value {
    let node = |kind: &str, nodes: &mut Vec<serde_json::Value>, parents: &mut Vec<usize>, build: &dyn Fn(&mut Vec<serde_json::Value>, &mut Vec<usize>) -> serde_json::Value| {
        let index = nodes.len();
        nodes.push(serde_json::Value::Null);
        parents.push(index);
        let data = build(nodes, parents);
        parents.pop();
        nodes[index] = json!({ "kind": kind, "data": data });
        index
    };
    let reference = match value {
        Value::Undefined => return json!({ "undefined": true }),
        Value::Null => return serde_json::Value::Null,
        Value::Bool(value) => return json!(value),
        Value::Number(number) if number.is_nan() => return json!({ "number": "NaN" }),
        Value::Number(number) if number.is_infinite() => {
            return json!({ "number": if *number > 0.0 { "Infinity" } else { "-Infinity" } });
        }
        Value::Number(number) if *number == 0.0 && number.is_sign_negative() => return json!({ "number": "-0" }),
        // Whole numbers as integers, as JavaScript writes them.
        Value::Number(number) if number.fract() == 0.0 && number.abs() < 9.0e15 => return json!(*number as i64),
        Value::Number(number) => return json!(number),
        Value::Text(text) => return json!(text),
        Value::Parent => return parents.last().map_or(serde_json::Value::Null, |index| json!({ "ref": index })),
        Value::Bytes(bytes) => {
            nodes.push(json!({ "kind": "bytes", "data": bytes }));
            nodes.len() - 1
        }
        Value::Array(items) => node("array", nodes, parents, &|nodes, parents| {
            items.iter().map(|item| encode_value(item, nodes, parents)).collect()
        }),
        Value::Object(entries) | Value::Error(_, entries) => {
            let index = node("object", nodes, parents, &|nodes, parents| {
                entries.iter().map(|(key, value)| json!([key, encode_value(value, nodes, parents)])).collect()
            });
            if let Value::Error(name, _) = value {
                nodes[index]["error"] = json!(name);
            }
            index
        }
    };
    json!({ "ref": reference })
}

/// Decode graph text. A reference back into a node being decoded (a cycle) decodes as `Null`.
pub(crate) fn decode(text: &str) -> Option<Value> {
    let graph: serde_json::Value = serde_json::from_str(text).ok()?;
    let nodes = graph.get("nodes")?.as_array()?;
    fn visit(value: &serde_json::Value, nodes: &[serde_json::Value], open: &mut Vec<usize>) -> Value {
        match value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(value) => Value::Bool(*value),
            serde_json::Value::Number(value) => Value::Number(value.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(value) => Value::Text(value.clone()),
            serde_json::Value::Object(object) => {
                if let Some(index) = object.get("ref").and_then(serde_json::Value::as_u64) {
                    let index = index as usize;
                    let Some(node) = nodes.get(index) else { return Value::Null };
                    if open.contains(&index) || open.len() > 64 {
                        return Value::Null;
                    }
                    open.push(index);
                    let data = node.get("data").and_then(serde_json::Value::as_array);
                    let decoded = match node.get("kind").and_then(serde_json::Value::as_str) {
                        Some("bytes") => Value::Bytes(
                            data.map(|bytes| bytes.iter().map(|byte| byte.as_u64().unwrap_or(0) as u8).collect())
                                .unwrap_or_default(),
                        ),
                        Some("array") => Value::Array(
                            data.map(|items| items.iter().map(|item| visit(item, nodes, open)).collect()).unwrap_or_default(),
                        ),
                        _ => {
                            let entries = data
                                .map(|entries| {
                                    entries
                                        .iter()
                                        .filter_map(|entry| {
                                            let entry = entry.as_array()?;
                                            Some((entry.first()?.as_str()?.to_owned(), visit(entry.get(1)?, nodes, open)))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            match node.get("error").and_then(serde_json::Value::as_str) {
                                Some(name) => Value::Error(name.to_owned(), entries),
                                None => Value::Object(entries),
                            }
                        }
                    };
                    open.pop();
                    return decoded;
                }
                if let Some(number) = object.get("number").and_then(serde_json::Value::as_str) {
                    return Value::Number(number.parse().unwrap_or(f64::NAN));
                }
                Value::Undefined
            }
            _ => Value::Null,
        }
    }
    Some(visit(graph.get("root")?, nodes, &mut Vec::new()))
}

/// An array of strings.
pub(crate) fn strings(items: impl IntoIterator<Item = String>) -> Value {
    Value::Array(items.into_iter().map(Value::Text).collect())
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

    #[test]
    fn a_self_signed_certificate_encodes_as_a_cycle() {
        let certificate = object([("raw", Value::Bytes(vec![1])), ("issuerCertificate", Value::Parent)]);
        let text = certificate.encode();
        assert_eq!(
            text,
            r#"{"nodes":[{"data":[["raw",{"ref":1}],["issuerCertificate",{"ref":0}]],"kind":"object"},{"data":[1],"kind":"bytes"}],"root":{"ref":0}}"#
        );
        // Decoding cuts the cycle.
        let decoded = decode(&text).unwrap();
        assert_eq!(decoded.get("issuerCertificate"), Some(&Value::Null));
        assert_eq!(decoded.get("raw"), Some(&Value::Bytes(vec![1])));
    }

    #[test]
    fn errors_and_special_values_round_trip() {
        let value = Value::Array(vec![
            error(Some("ECONNRESET"), "reset"),
            Value::Undefined,
            Value::Number(f64::INFINITY),
            Value::Number(3.0),
            Value::Number(0.5),
        ]);
        let decoded = decode(&value.encode()).unwrap();
        assert_eq!(decoded, value);
        assert!(value.encode().contains(r#""error":"Error""#));
    }
}
