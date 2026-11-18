//! JSON at the edge of a component call.
//!
//! `jco-node-run` takes an export's arguments as JSON and prints its result as JSON, using the
//! same value mapping Jco's JavaScript bindings use, so a test written against a transpiled
//! component reads the same against this host: record fields are camelCase, an `option` is the
//! value or `null`, a `variant` is `{ "tag", "val" }`, and a `result` is the payload on success.

use anyhow::{Context as _, anyhow, bail};
use serde_json::{Map, Number, Value};
use wash_runtime::wasmtime::component::{Val, types::Type};

/// `kebab-case` (WIT) to `camelCase` (JavaScript).
pub fn camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for character in name.chars() {
        if character == '-' {
            upper = true;
        } else if upper {
            out.extend(character.to_uppercase());
            upper = false;
        } else {
            out.push(character);
        }
    }
    out
}

fn float(value: f64) -> Value {
    // JSON has no NaN or infinities; JavaScript's `JSON.stringify` writes them as null.
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// Convert a value a component returned into JSON.
pub fn to_json(value: &Val) -> anyhow::Result<Value> {
    Ok(match value {
        Val::Bool(v) => Value::Bool(*v),
        Val::S8(v) => Value::from(*v),
        Val::U8(v) => Value::from(*v),
        Val::S16(v) => Value::from(*v),
        Val::U16(v) => Value::from(*v),
        Val::S32(v) => Value::from(*v),
        Val::U32(v) => Value::from(*v),
        // JavaScript bindings give 64-bit integers as BigInt, which has no JSON form; a decimal
        // string keeps every bit.
        Val::S64(v) => Value::String(v.to_string()),
        Val::U64(v) => Value::String(v.to_string()),
        Val::Float32(v) => float(f64::from(*v)),
        Val::Float64(v) => float(*v),
        Val::Char(v) => Value::String(v.to_string()),
        Val::String(v) => Value::String(v.clone()),
        Val::List(items) => {
            // `list<u8>` is a byte array either way, but tuples of pairs stay arrays of arrays.
            Value::Array(items.iter().map(to_json).collect::<anyhow::Result<_>>()?)
        }
        Val::Record(fields) => {
            let mut object = Map::new();
            for (name, value) in fields {
                object.insert(camel_case(name), to_json(value)?);
            }
            Value::Object(object)
        }
        Val::Tuple(items) => Value::Array(items.iter().map(to_json).collect::<anyhow::Result<_>>()?),
        Val::Variant(tag, payload) => {
            let mut object = Map::new();
            object.insert("tag".into(), Value::String(tag.clone()));
            if let Some(payload) = payload {
                object.insert("val".into(), to_json(payload)?);
            }
            Value::Object(object)
        }
        Val::Enum(tag) => Value::String(tag.clone()),
        Val::Option(inner) => match inner {
            Some(inner) => to_json(inner)?,
            None => Value::Null,
        },
        Val::Result(result) => match result {
            Ok(payload) => payload.as_deref().map(to_json).transpose()?.unwrap_or(Value::Null),
            Err(payload) => {
                let mut object = Map::new();
                object.insert("tag".into(), Value::String("err".into()));
                object.insert(
                    "val".into(),
                    payload.as_deref().map(to_json).transpose()?.unwrap_or(Value::Null),
                );
                Value::Object(object)
            }
        },
        Val::Flags(names) => {
            let mut object = Map::new();
            for name in names {
                object.insert(camel_case(name), Value::Bool(true));
            }
            Value::Object(object)
        }
        other => bail!("a {other:?} cannot be written as JSON"),
    })
}

fn integer<T: TryFrom<i128>>(value: &Value, ty: &str) -> anyhow::Result<T> {
    let wide: i128 = match value {
        Value::Number(number) => {
            if let Some(v) = number.as_i64() {
                i128::from(v)
            } else if let Some(v) = number.as_u64() {
                i128::from(v)
            } else {
                bail!("{value} is not an integer {ty}")
            }
        }
        Value::String(text) => text.parse().with_context(|| format!("{text:?} is not an integer {ty}"))?,
        _ => bail!("{value} is not a {ty}"),
    };
    T::try_from(wide).map_err(|_| anyhow!("{wide} is out of range for {ty}"))
}

/// Convert a JSON argument into a value of the WIT type an export expects.
pub fn from_json(value: &Value, ty: &Type) -> anyhow::Result<Val> {
    Ok(match ty {
        Type::Bool => Val::Bool(value.as_bool().with_context(|| format!("{value} is not a bool"))?),
        Type::S8 => Val::S8(integer(value, "s8")?),
        Type::U8 => Val::U8(integer(value, "u8")?),
        Type::S16 => Val::S16(integer(value, "s16")?),
        Type::U16 => Val::U16(integer(value, "u16")?),
        Type::S32 => Val::S32(integer(value, "s32")?),
        Type::U32 => Val::U32(integer(value, "u32")?),
        Type::S64 => Val::S64(integer(value, "s64")?),
        Type::U64 => Val::U64(integer(value, "u64")?),
        Type::Float32 => Val::Float32(value.as_f64().with_context(|| format!("{value} is not an f32"))? as f32),
        Type::Float64 => Val::Float64(value.as_f64().with_context(|| format!("{value} is not an f64"))?),
        Type::Char => {
            let text = value.as_str().with_context(|| format!("{value} is not a char"))?;
            let mut characters = text.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) => Val::Char(character),
                _ => bail!("{text:?} is not a single char"),
            }
        }
        Type::String => Val::String(value.as_str().with_context(|| format!("{value} is not a string"))?.to_owned()),
        Type::List(list) => {
            let element = list.ty();
            let items = value.as_array().with_context(|| format!("{value} is not a list"))?;
            Val::List(items.iter().map(|item| from_json(item, &element)).collect::<anyhow::Result<_>>()?)
        }
        Type::Record(record) => {
            let object = value.as_object().with_context(|| format!("{value} is not a record"))?;
            let mut fields = Vec::new();
            for field in record.fields() {
                let given = object.get(&camel_case(field.name)).or_else(|| object.get(field.name));
                let given = given.unwrap_or(&Value::Null);
                fields.push((
                    field.name.to_owned(),
                    from_json(given, &field.ty).with_context(|| format!("in field {}", field.name))?,
                ));
            }
            Val::Record(fields)
        }
        Type::Tuple(tuple) => {
            let items = value.as_array().with_context(|| format!("{value} is not a tuple"))?;
            let types: Vec<Type> = tuple.types().collect();
            if items.len() != types.len() {
                bail!("a tuple of {} was given {} values", types.len(), items.len());
            }
            Val::Tuple(
                items.iter().zip(&types).map(|(item, ty)| from_json(item, ty)).collect::<anyhow::Result<_>>()?,
            )
        }
        Type::Variant(variant) => {
            let tag = value.get("tag").and_then(Value::as_str).with_context(|| format!("{value} has no tag"))?;
            let case = variant.cases().find(|case| case.name == tag).with_context(|| format!("no case {tag}"))?;
            let payload = match case.ty {
                Some(ty) => Some(Box::new(from_json(value.get("val").unwrap_or(&Value::Null), &ty)?)),
                None => None,
            };
            Val::Variant(tag.to_owned(), payload)
        }
        Type::Enum(_) => Val::Enum(value.as_str().with_context(|| format!("{value} is not an enum case"))?.to_owned()),
        Type::Option(option) => match value {
            Value::Null => Val::Option(None),
            other => Val::Option(Some(Box::new(from_json(other, &option.ty())?))),
        },
        Type::Result(result) => {
            let is_err = value.get("tag").and_then(Value::as_str) == Some("err");
            let payload = |ty: Option<Type>, source: &Value| -> anyhow::Result<Option<Box<Val>>> {
                ty.map(|ty| from_json(source, &ty).map(Box::new)).transpose()
            };
            if is_err {
                Val::Result(Err(payload(result.err(), value.get("val").unwrap_or(&Value::Null))?))
            } else {
                let source = match value.get("tag").and_then(Value::as_str) {
                    Some("ok") => value.get("val").unwrap_or(&Value::Null),
                    _ => value,
                };
                Val::Result(Ok(payload(result.ok(), source)?))
            }
        }
        Type::Flags(flags) => {
            let object = value.as_object().with_context(|| format!("{value} is not a flags object"))?;
            Val::Flags(
                flags
                    .names()
                    .filter(|name| {
                        object.get(&camel_case(name)).or_else(|| object.get(*name)).and_then(Value::as_bool)
                            == Some(true)
                    })
                    .map(str::to_owned)
                    .collect(),
            )
        }
        other => bail!("an argument of type {other:?} cannot be given as JSON"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_javascript_bindings() {
        assert_eq!(camel_case("sync-contents"), "syncContents");
        assert_eq!(camel_case("run"), "run");
        assert_eq!(camel_case("atime-ms"), "atimeMs");
    }

    #[test]
    fn records_options_and_results_flatten() {
        let value = Val::Record(vec![
            ("is-file".into(), Val::Bool(true)),
            ("missing".into(), Val::Option(None)),
            ("size".into(), Val::U64(1 << 60)),
            ("outcome".into(), Val::Result(Ok(Some(Box::new(Val::String("fine".into())))))),
        ]);
        assert_eq!(
            to_json(&value).unwrap(),
            serde_json::json!({ "isFile": true, "missing": null, "size": "1152921504606846976", "outcome": "fine" })
        );
    }
}
