//! `pg-value` ⇄ JSON.
//!
//! The host hands rows back as `wasmcloud:postgres/types.pg-value` variants
//! (see `wit/deps/wasmcloud-postgres-0.2.0/package.wit`); tools render them as
//! JSON, and tool parameters arrive as JSON that has to become `pg-value`s.
//! Both directions live here, pure and host-free, so the rules are in one
//! place:
//!
//! - **Rows → JSON** ([`to_json`]): integers and floats become JSON numbers
//!   (non-finite floats become the strings `"NaN"`, `"Infinity"`,
//!   `"-Infinity"`), `numeric`/`money` stay strings (exact), `bytea` is
//!   base64, dates/times are ISO-8601 strings (`timestamptz` with a `Z` or
//!   `±HH:MM` suffix), `json`/`jsonb` are parsed, arrays are JSON arrays,
//!   `hstore` is an object, `bit`/`varbit` are `{bits, hex}`.
//! - **JSON → params** ([`param_from_json`]): a bare JSON scalar maps to the
//!   type the daemon sends with no hints (`string`→`text`, integer→`int8`,
//!   number→`float8`, bool→`bool`, null→`null`; arrays/objects are typed by
//!   their content); a `{"type": "...", "value": ...}` object picks a Postgres
//!   type explicitly — the way to bind `uuid`, `int4`, `timestamptz`,
//!   `numeric`, `bytea` … columns without SQL casts.
//!
//! The `hashable-f64` encoding (`(mantissa, exponent, sign)`, from
//! `num_traits::Float::integer_decode`) is reconstructed exactly here,
//! including subnormals, infinities and NaN.

use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::postgres::bindings::wasmcloud::postgres::types::{
    Date, HashableF64, Interval, Lexeme, LexemeWeight, MacAddressEui48, MacAddressEui64, Offset,
    PgValue, Time, TimeTz, Timestamp, TimestampTz,
};

/// Longest string a single cell may carry in a tool result; longer text is
/// cut on a character boundary and marked. Keeps one wide `text`/`jsonb`
/// column from flooding the model's context.
pub const MAX_CELL_CHARS: usize = 32 * 1024;

// ── hashable-f64 ────────────────────────────────────────────────────────────

/// `num_traits::Float::integer_decode` for `f64`: `(mantissa, exponent, sign)`
/// with `value = sign * mantissa * 2^exponent`.
pub fn encode_f64(value: f64) -> HashableF64 {
    let bits = value.to_bits();
    let sign: i8 = if bits >> 63 == 0 { 1 } else { -1 };
    let mut exponent: i16 = ((bits >> 52) & 0x7ff) as i16;
    let mantissa = if exponent == 0 {
        (bits & 0x000f_ffff_ffff_ffff) << 1
    } else {
        (bits & 0x000f_ffff_ffff_ffff) | 0x0010_0000_0000_0000
    };
    exponent -= 1023 + 52;
    (mantissa, exponent, sign)
}

/// `integer_decode` for `f32` (what the host uses for `float4`/`real`).
pub fn encode_f32(value: f32) -> HashableF64 {
    let bits = value.to_bits();
    let sign: i8 = if bits >> 31 == 0 { 1 } else { -1 };
    let mut exponent: i16 = ((bits >> 23) & 0xff) as i16;
    let mantissa = if exponent == 0 {
        (bits & 0x7f_ffff) << 1
    } else {
        (bits & 0x7f_ffff) | 0x80_0000
    };
    exponent -= 127 + 23;
    (u64::from(mantissa), exponent, sign)
}

/// Rebuilds the `f64` an `integer_decode` triple came from. Exact for every
/// finite value (scaling by powers of two is exact as long as it does not
/// under/overflow, and the stepwise scaling below never does before the final
/// step), and recognises the infinity/NaN encodings of both `f64` and `f32`.
pub fn decode_f64((mantissa, exponent, sign): HashableF64) -> f64 {
    let signum = if sign < 0 { -1.0 } else { 1.0 };
    if mantissa == 0 {
        return 0.0 * signum;
    }
    // f64 non-finite: exponent field 0x7ff → 2047 - 1075 = 972, mantissa has
    // the implicit bit (2^52) set. f32 non-finite: 255 - 150 = 105, 2^23.
    if (exponent == 972 && mantissa >= 1 << 52) || (exponent == 105 && mantissa >= 1 << 23) {
        let is_inf = mantissa == 1 << 52 || mantissa == 1 << 23;
        return if is_inf {
            f64::INFINITY * signum
        } else {
            f64::NAN
        };
    }
    let mut value = mantissa as f64;
    let mut exponent = i32::from(exponent);
    while exponent > 0 {
        let step = exponent.min(1000);
        value *= 2f64.powi(step);
        exponent -= step;
    }
    while exponent < 0 {
        let step = (-exponent).min(1000);
        value /= 2f64.powi(step);
        exponent += step;
    }
    value * signum
}

fn float_json(value: f64) -> Value {
    if value.is_nan() {
        Value::String("NaN".into())
    } else if value == f64::INFINITY {
        Value::String("Infinity".into())
    } else if value == f64::NEG_INFINITY {
        Value::String("-Infinity".into())
    } else {
        // Finite: serde_json represents every finite f64.
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

// ── date/time rendering ─────────────────────────────────────────────────────

fn date_string(date: &Date) -> String {
    match date {
        Date::PositiveInfinity => "infinity".into(),
        Date::NegativeInfinity => "-infinity".into(),
        Date::Ymd((year, month, day)) => {
            if *year <= 0 {
                // Postgres has no year 0: year -1 is "0002 BC".
                format!("{:04}-{:02}-{:02} BC", 1 - year, month, day)
            } else {
                format!("{year:04}-{month:02}-{day:02}")
            }
        }
    }
}

fn time_string(time: &Time) -> String {
    if time.micro == 0 {
        format!("{:02}:{:02}:{:02}", time.hour, time.min, time.sec)
    } else {
        format!(
            "{:02}:{:02}:{:02}.{:06}",
            time.hour, time.min, time.sec, time.micro
        )
    }
}

fn timestamp_string(ts: &Timestamp) -> String {
    match ts.date {
        Date::PositiveInfinity | Date::NegativeInfinity => date_string(&ts.date),
        Date::Ymd(_) => format!("{}T{}", date_string(&ts.date), time_string(&ts.time)),
    }
}

fn offset_seconds(offset: &Offset) -> i32 {
    match offset {
        Offset::EasternHemisphereSecs(secs) => *secs,
        Offset::WesternHemisphereSecs(secs) => -secs,
    }
}

fn timestamptz_string(ts: &TimestampTz) -> String {
    let base = timestamp_string(&ts.timestamp);
    if matches!(
        ts.timestamp.date,
        Date::PositiveInfinity | Date::NegativeInfinity
    ) {
        return base;
    }
    let secs = offset_seconds(&ts.offset);
    if secs == 0 {
        format!("{base}Z")
    } else {
        let sign = if secs < 0 { '-' } else { '+' };
        let abs = secs.unsigned_abs();
        format!("{base}{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
    }
}

fn timetz_string(t: &TimeTz) -> String {
    format!("{}{}", time_string(&t.time), t.timesonze)
}

/// The WIT `interval` record is a date range (unrelated to Postgres
/// `interval`, which the host does not convert); rendered structurally.
fn interval_json(iv: &Interval) -> Value {
    json!({
        "start": date_string(&iv.start),
        "start_inclusive": iv.start_inclusive,
        "end": date_string(&iv.end),
        "end_inclusive": iv.end_inclusive,
    })
}

// ── misc rendering ──────────────────────────────────────────────────────────

fn mac48_string(m: &MacAddressEui48) -> String {
    let b = m.bytes;
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b.0, b.1, b.2, b.3, b.4, b.5
    )
}

fn mac64_string(m: &MacAddressEui64) -> String {
    let b = m.bytes;
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b.0, b.1, b.2, b.3, b.4, b.5, b.6, b.7
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn bits_json(bits: Option<u32>, bytes: &[u8]) -> Value {
    json!({ "bits": bits, "hex": hex(bytes) })
}

fn point_json(p: &(HashableF64, HashableF64)) -> Value {
    json!([decode_f64(p.0), decode_f64(p.1)])
}

fn points_json(points: &[(HashableF64, HashableF64)]) -> Value {
    Value::Array(points.iter().map(point_json).collect())
}

fn segment_json(s: &((HashableF64, HashableF64), (HashableF64, HashableF64))) -> Value {
    json!([point_json(&s.0), point_json(&s.1)])
}

fn text_from_bytes(bytes: &[u8]) -> Value {
    bounded_text(String::from_utf8_lossy(bytes).into_owned())
}

/// Caps one text cell at [`MAX_CELL_CHARS`] characters (UTF-8 safe).
fn bounded_text(mut s: String) -> Value {
    let chars = s.chars().count();
    if chars > MAX_CELL_CHARS {
        let cut = s
            .char_indices()
            .nth(MAX_CELL_CHARS)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        s.truncate(cut);
        s.push_str(&format!(
            "…[truncated: {} of {chars} chars shown]",
            MAX_CELL_CHARS
        ));
    }
    Value::String(s)
}

fn json_cell(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(_) => bounded_text(text.to_owned()),
    }
}

fn lexeme_json(l: &Lexeme) -> Value {
    json!({
        "data": l.data,
        "position": l.position,
        "weight": l.weight.map(|w| match w {
            LexemeWeight::A => "A",
            LexemeWeight::B => "B",
            LexemeWeight::C => "C",
            LexemeWeight::D => "D",
        }),
    })
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn list<T>(items: &[T], f: impl Fn(&T) -> Value) -> Value {
    Value::Array(items.iter().map(f).collect())
}

/// A cell the host delivered in a form that cannot be rendered truthfully.
#[derive(Debug, Clone)]
pub struct CellError {
    pub reason: String,
}

/// Renders one cell.
///
/// `Err` only for `numeric`/`decimal`/`money` cells the host mis-decoded
/// (see [`numeric_cell`]) — rendering those as strings would be silent data
/// corruption, so the tool fails the whole result with a cast hint instead.
pub fn to_json(value: &PgValue) -> Result<Value, CellError> {
    Ok(match value {
        PgValue::Null => Value::Null,
        PgValue::BigInt(n) | PgValue::Int8(n) | PgValue::BigSerial(n) | PgValue::Serial8(n) => {
            json!(n)
        }
        PgValue::Int8Array(ns) => json!(ns),
        PgValue::Bool(b) | PgValue::Boolean(b) => json!(b),
        PgValue::BoolArray(bs) => json!(bs),
        PgValue::Double(f) | PgValue::Float8(f) | PgValue::Real(f) | PgValue::Float4(f) => {
            float_json(decode_f64(*f))
        }
        PgValue::Float8Array(fs) | PgValue::Float4Array(fs) => {
            list(fs, |f| float_json(decode_f64(*f)))
        }
        PgValue::Integer(n) | PgValue::Int(n) | PgValue::Int4(n) => json!(n),
        PgValue::Int4Array(ns) => json!(ns),
        PgValue::Numeric(s) | PgValue::Decimal(s) => numeric_cell(s)?,
        PgValue::Money(s) => money_cell(s)?,
        PgValue::NumericArray(ss) => Value::Array(
            ss.iter()
                .map(|s| numeric_cell(s))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        PgValue::MoneyArray(ss) => Value::Array(
            ss.iter()
                .map(|s| money_cell(s))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        PgValue::Serial(n) | PgValue::Serial4(n) => json!(n),
        PgValue::SmallInt(n) | PgValue::Int2(n) | PgValue::SmallSerial(n) | PgValue::Serial2(n) => {
            json!(n)
        }
        PgValue::Int2Array(ns) | PgValue::Int2Vector(ns) => json!(ns),
        PgValue::Int2VectorArray(nss) => json!(nss),
        PgValue::Bit((n, bytes)) => bits_json(Some(*n), bytes),
        PgValue::BitArray(items) => list(items, |(n, bytes)| bits_json(Some(*n), bytes)),
        PgValue::BitVarying((n, bytes)) | PgValue::Varbit((n, bytes)) => bits_json(*n, bytes),
        PgValue::VarbitArray(items) => list(items, |(n, bytes)| bits_json(*n, bytes)),
        PgValue::Bytea(bytes) => json!(b64(bytes)),
        PgValue::ByteaArray(items) => list(items, |bytes| json!(b64(bytes))),
        PgValue::Char((_, bytes)) | PgValue::Varchar((_, bytes)) => text_from_bytes(bytes),
        PgValue::CharArray(items) => list(items, |(_, bytes)| text_from_bytes(bytes)),
        PgValue::VarcharArray(items) => list(items, |(_, bytes)| text_from_bytes(bytes)),
        PgValue::Cidr(s) | PgValue::Inet(s) => json!(s),
        PgValue::CidrArray(ss) | PgValue::InetArray(ss) => json!(ss),
        PgValue::Macaddr(m) => json!(mac48_string(m)),
        PgValue::MacaddrArray(ms) => list(ms, |m| json!(mac48_string(m))),
        PgValue::Macaddr8(m) => json!(mac64_string(m)),
        PgValue::Macaddr8Array(ms) => list(ms, |m| json!(mac64_string(m))),
        PgValue::Box(b) => segment_json(b),
        PgValue::BoxArray(bs) => list(bs, segment_json),
        PgValue::Circle((center, radius)) => {
            json!({ "center": point_json(center), "radius": decode_f64(*radius) })
        }
        PgValue::CircleArray(cs) => list(
            cs,
            |(center, radius)| json!({ "center": point_json(center), "radius": decode_f64(*radius) }),
        ),
        PgValue::Line(l) | PgValue::Lseg(l) => segment_json(l),
        PgValue::LineArray(ls) | PgValue::LsegArray(ls) => list(ls, segment_json),
        PgValue::Path(ps) | PgValue::Polygon(ps) => points_json(ps),
        PgValue::PathArray(pss) | PgValue::PolygonArray(pss) => list(pss, |ps| points_json(ps)),
        PgValue::Point(p) => point_json(p),
        PgValue::PointArray(ps) => points_json(ps),
        PgValue::Date(d) => json!(date_string(d)),
        PgValue::DateArray(ds) => list(ds, |d| json!(date_string(d))),
        PgValue::Interval(iv) => interval_json(iv),
        PgValue::IntervalArray(ivs) => list(ivs, interval_json),
        PgValue::Time(t) => json!(time_string(t)),
        PgValue::TimeArray(ts) => list(ts, |t| json!(time_string(t))),
        PgValue::TimeTz(t) => json!(timetz_string(t)),
        PgValue::TimeTzArray(ts) => list(ts, |t| json!(timetz_string(t))),
        PgValue::Timestamp(ts) => json!(timestamp_string(ts)),
        PgValue::TimestampArray(tss) => list(tss, |ts| json!(timestamp_string(ts))),
        PgValue::TimestampTz(ts) => json!(timestamptz_string(ts)),
        PgValue::TimestampTzArray(tss) => list(tss, |ts| json!(timestamptz_string(ts))),
        PgValue::Json(s) | PgValue::Jsonb(s) => json_cell(s),
        PgValue::JsonArray(ss) | PgValue::JsonbArray(ss) => list(ss, |s| json_cell(s)),
        PgValue::PgLsn(n) => json!(format!("{:X}/{:X}", n >> 32, n & 0xffff_ffff)),
        PgValue::PgLsnArray(ns) => list(ns, |n| {
            json!(format!("{:X}/{:X}", n >> 32, n & 0xffff_ffff))
        }),
        PgValue::PgSnapshot((xmin, xmax, xip)) => {
            json!({ "xmin": xmin, "xmax": xmax, "xip": xip })
        }
        PgValue::TxidSnapshot(n) => json!(n),
        PgValue::Name(s) | PgValue::Text(s) | PgValue::Xml(s) | PgValue::TsQuery(s) => {
            bounded_text(s.clone())
        }
        PgValue::NameArray(ss) | PgValue::TextArray(ss) | PgValue::XmlArray(ss) => {
            list(ss, |s| bounded_text(s.clone()))
        }
        PgValue::TsVector(ls) => list(ls, lexeme_json),
        PgValue::Uuid(s) => json!(s),
        PgValue::UuidArray(ss) => json!(ss),
        PgValue::Hstore(pairs) => {
            let mut map = Map::with_capacity(pairs.len());
            for (k, v) in pairs {
                map.insert(
                    k.clone(),
                    v.clone().map(Value::String).unwrap_or(Value::Null),
                );
            }
            Value::Object(map)
        }
    })
}

/// Whether `s` is a decimal literal Postgres would print (`-12.50`, `1e3`,
/// `NaN`, `Infinity`).
fn is_numeric_literal(s: &str) -> bool {
    let t = s.trim();
    if matches!(t, "NaN" | "Infinity" | "-Infinity") {
        return true;
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    !body.is_empty()
        && body.chars().any(|c| c.is_ascii_digit())
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
}

/// The host maps `numeric` columns to a string by reading the *binary* wire
/// bytes as UTF-8 — a gap in its conversion (`Numeric = String`,
/// `String::from_sql` on `NUMERIC`). Values whose bytes happen to be valid
/// UTF-8 arrive as garbage strings; the rest fail the whole query. When the
/// bytes survived, decode the wire format here (base-10000 digits, weight,
/// sign, dscale) so the caller gets the exact decimal; a value that is
/// neither a literal nor decodable is an error, never a garbage string.
fn numeric_cell(s: &str) -> Result<Value, CellError> {
    if is_numeric_literal(s) {
        return Ok(Value::String(s.trim().to_owned()));
    }
    decode_numeric_wire(s.as_bytes())
        .map(Value::String)
        .ok_or_else(|| CellError {
            reason: "a numeric/decimal value could not be decoded from the host's representation"
                .to_owned(),
        })
}

/// `money`: 8-byte big-endian integer of hundredths (assumes `lc_monetary`
/// with two fractional digits).
fn money_cell(s: &str) -> Result<Value, CellError> {
    if is_numeric_literal(s) {
        return Ok(Value::String(s.trim().to_owned()));
    }
    let bytes = s.as_bytes();
    if bytes.len() == 8 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        let cents = i64::from_be_bytes(raw);
        let sign = if cents < 0 { "-" } else { "" };
        let abs = cents.unsigned_abs();
        return Ok(Value::String(format!(
            "{sign}{}.{:02}",
            abs / 100,
            abs % 100
        )));
    }
    Err(CellError {
        reason: "a money value could not be decoded from the host's representation".to_owned(),
    })
}

/// Decodes Postgres' binary `numeric` send format.
fn decode_numeric_wire(b: &[u8]) -> Option<String> {
    if b.len() < 8 || !b.len().is_multiple_of(2) {
        return None;
    }
    let rd = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
    let ndigits = usize::from(rd(0));
    let weight = i32::from(rd(2) as i16);
    let sign = rd(4);
    let dscale = usize::from(rd(6));
    if b.len() != 8 + 2 * ndigits || dscale > 16_383 {
        return None;
    }
    let negative = match sign {
        0x0000 => false,
        0x4000 => true,
        0xC000 => return (ndigits == 0).then(|| "NaN".to_owned()),
        0xD000 => return Some("Infinity".to_owned()),
        0xF000 => return Some("-Infinity".to_owned()),
        _ => return None,
    };
    let digits: Vec<u16> = (0..ndigits).map(|i| rd(8 + 2 * i)).collect();
    if digits.iter().any(|d| *d >= 10_000) {
        return None;
    }
    let mut int_part = String::new();
    if weight >= 0 {
        for i in 0..=(weight as usize) {
            let d = digits.get(i).copied().unwrap_or(0);
            if i == 0 {
                int_part.push_str(&d.to_string());
            } else {
                int_part.push_str(&format!("{d:04}"));
            }
        }
    } else {
        int_part.push('0');
    }
    let mut frac = String::new();
    if weight < -1 {
        for _ in 0..(-weight - 1) {
            frac.push_str("0000");
        }
    }
    let start = if weight >= 0 { weight as usize + 1 } else { 0 };
    for d in digits.iter().skip(start) {
        frac.push_str(&format!("{d:04}"));
    }
    while frac.len() < dscale {
        frac.push('0');
    }
    frac.truncate(dscale);
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&int_part);
    if dscale > 0 {
        out.push('.');
        out.push_str(&frac);
    }
    Some(out)
}

// ── JSON → pg-value (parameters) ────────────────────────────────────────────

/// How a parameter was typed, so a bind failure can be retried with another
/// encoding only where the caller left the choice to us.
///
/// The host binds every parameter in Postgres' *binary* format with no type
/// hints; Postgres infers each `$N`'s type from the statement and then reads
/// the bytes as that type. Too many bytes fail with `22P03` ("incorrect
/// binary data format in bind parameter N" — it names the parameter); too
/// few fail with `08P01` ("insufficient data left in message" — it does
/// not); and equal length is read *silently* whatever the type (8 bytes of
/// `int8` are a garbage `float8`; 8 bytes into a `numeric` slot are `0`).
/// So each chain starts with its longest encoding and only ever shrinks:
/// every rejection along the way names its parameter, and the only silent
/// misreads left are the same-length pairs `int8`/`float8` and
/// `int4`/`float4` (a bare integer against a float column, or a bare float
/// against an integer column) — pass floats as floats, or a typed param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Typing {
    /// Bare JSON integer → `numeric`; retried as `int8`, `int4`, `int2`
    /// (each only when the value fits).
    AutoInteger,
    /// Bare JSON float → `numeric`; retried as `float8`.
    AutoFloat,
    /// Bare JSON string → `text`; retried as whatever the string parses as,
    /// longest encoding first (uuid, numeric, timestamptz/timestamp/time/
    /// int8/float8, date/int4, int2, bool).
    AutoText,
    /// Everything else (null, bool, arrays, objects, explicit `{"type": …}`).
    Fixed,
}

/// A parameter ready for the host plus how it was typed.
#[derive(Debug, Clone)]
pub struct Param {
    pub value: PgValue,
    pub typing: Typing,
    /// The original JSON, kept so a retry can re-encode it.
    pub source: Value,
    /// Retry candidates already consumed (0 = the first encoding).
    pub step: u8,
}

impl Param {
    /// A fixed (never retried) parameter.
    pub fn fixed(value: PgValue, source: Value) -> Self {
        Self {
            value,
            typing: Typing::Fixed,
            source,
            step: 0,
        }
    }
}

/// Converts one JSON argument (1-based `index`, for messages) to a parameter.
pub fn param_from_json(index: usize, value: &Value) -> Result<Param, String> {
    let (pg, typing) = match value {
        Value::Null => (PgValue::Null, Typing::Fixed),
        Value::Bool(b) => (PgValue::Bool(*b), Typing::Fixed),
        Value::Number(n) => {
            if n.as_i64().is_some() {
                (PgValue::Numeric(n.to_string()), Typing::AutoInteger)
            } else if let Some(u) = n.as_u64() {
                (PgValue::Numeric(u.to_string()), Typing::Fixed)
            } else if n.as_f64().is_some() {
                (PgValue::Numeric(n.to_string()), Typing::AutoFloat)
            } else {
                return Err(format!("param {index}: unsupported number {n}"));
            }
        }
        Value::String(s) => (PgValue::Text(s.clone()), Typing::AutoText),
        Value::Array(items) => (auto_array(index, items)?, Typing::Fixed),
        Value::Object(map) => match (map.get("type"), map.get("value")) {
            (Some(Value::String(ty)), Some(inner)) if map.len() == 2 => {
                (typed_param(index, ty, inner)?, Typing::Fixed)
            }
            _ => (PgValue::Jsonb(value.to_string()), Typing::Fixed),
        },
    };
    Ok(Param {
        value: pg,
        typing,
        source: value.clone(),
        step: 0,
    })
}

/// The encodings to try, in order, after the first one was rejected —
/// strictly non-increasing in wire length (see [`Typing`]).
fn retry_candidates(typing: Typing, source: &Value) -> Vec<PgValue> {
    let mut out = Vec::new();
    match (typing, source) {
        (Typing::AutoInteger, Value::Number(n)) => {
            let Some(v) = n.as_i64() else {
                return out;
            };
            out.push(PgValue::Int8(v));
            if let Ok(small) = i32::try_from(v) {
                out.push(PgValue::Int4(small));
            }
            if let Ok(small) = i16::try_from(v) {
                out.push(PgValue::Int2(small));
            }
        }
        (Typing::AutoFloat, Value::Number(n)) => {
            if let Some(v) = n.as_f64() {
                out.push(PgValue::Float8(encode_f64(v)));
            }
        }
        (Typing::AutoText, Value::String(s)) => {
            let t = s.trim();
            // 16 bytes.
            if is_uuid(t) {
                out.push(PgValue::Uuid(t.to_owned()));
            }
            // numeric: 8 + 2 * digit groups (>= 10 for any non-zero value).
            let as_int = t.parse::<i64>().ok();
            let as_float = t.parse::<f64>().ok().filter(|f| f.is_finite());
            if as_int.is_some() || (as_float.is_some() && numeric_text(source).is_some()) {
                out.push(PgValue::Numeric(t.to_owned()));
            }
            // 8 bytes.
            if let Some((ts, offset)) = parse_timestamp(t) {
                match offset {
                    Some(secs) => {
                        out.push(PgValue::TimestampTz(to_utc(ts, secs)));
                        out.push(PgValue::Timestamp(ts));
                    }
                    None => {
                        out.push(PgValue::Timestamp(ts));
                        out.push(PgValue::TimestampTz(to_utc(ts, 0)));
                    }
                }
            } else if let Some((time, _)) = parse_time(t) {
                out.push(PgValue::Time(time));
            }
            if let Some(v) = as_int {
                out.push(PgValue::Int8(v));
            } else if let Some(v) = as_float {
                out.push(PgValue::Float8(encode_f64(v)));
            }
            // 4 bytes.
            let date_only = !t.contains(['T', 't', ' ']) && !t.contains(':');
            if date_only {
                if let Some((ts, _)) = parse_timestamp(t) {
                    out.push(PgValue::Date(ts.date));
                }
            }
            if let Some(small) = as_int.and_then(|v| i32::try_from(v).ok()) {
                out.push(PgValue::Int4(small));
            }
            // 2 bytes.
            if let Some(small) = as_int.and_then(|v| i16::try_from(v).ok()) {
                out.push(PgValue::Int2(small));
            }
            // 1 byte.
            if let Some(b) = as_bool(source) {
                out.push(PgValue::Bool(b));
            }
        }
        _ => {}
    }
    out
}

/// Re-encodes an auto-typed parameter with its next candidate type after the
/// database rejected the previous encoding. `None` when there is nothing
/// left to try.
pub fn retry_param(param: &Param) -> Option<Param> {
    let next = retry_candidates(param.typing, &param.source)
        .into_iter()
        .nth(usize::from(param.step))?;
    Some(Param {
        value: next,
        typing: param.typing,
        source: param.source.clone(),
        step: param.step.saturating_add(1),
    })
}

/// Human description of an auto-typed parameter's encoding attempts, for the
/// error a caller sees when every candidate was rejected.
pub fn describe_attempts(param: &Param) -> String {
    let first = match param.typing {
        Typing::AutoInteger | Typing::AutoFloat => "numeric",
        Typing::AutoText => "text",
        Typing::Fixed => pg_type_name(&param.value),
    };
    let names: Vec<&str> = retry_candidates(param.typing, &param.source)
        .iter()
        .take(usize::from(param.step))
        .map(pg_type_name)
        .collect();
    if names.is_empty() {
        format!("sent as {first}")
    } else {
        format!("sent as {first}, then retried as {}", names.join(", "))
    }
}

/// Postgres type name of an encoding, for messages.
pub fn pg_type_name(value: &PgValue) -> &'static str {
    match value {
        PgValue::Null => "null",
        PgValue::Int2(_) => "int2",
        PgValue::Int4(_) => "int4",
        PgValue::Int8(_) => "int8",
        PgValue::Numeric(_) => "numeric",
        PgValue::Float4(_) => "float4",
        PgValue::Float8(_) => "float8",
        PgValue::Text(_) => "text",
        PgValue::Bool(_) => "bool",
        PgValue::Uuid(_) => "uuid",
        PgValue::Date(_) => "date",
        PgValue::Time(_) => "time",
        PgValue::Timestamp(_) => "timestamp",
        PgValue::TimestampTz(_) => "timestamptz",
        PgValue::Json(_) => "json",
        PgValue::Jsonb(_) => "jsonb",
        PgValue::Bytea(_) => "bytea",
        PgValue::TextArray(_) => "text[]",
        PgValue::Int8Array(_) => "int8[]",
        PgValue::Int4Array(_) => "int4[]",
        PgValue::BoolArray(_) => "bool[]",
        PgValue::Float8Array(_) => "float8[]",
        _ => "other",
    }
}

/// Names of the explicit parameter types [`typed_param`] accepts, for
/// messages and docs.
pub const TYPED_PARAM_TYPES: &str = "int2, int4, int8/bigint, float4, float8, numeric/decimal, \
     text/varchar, bool, uuid, json, jsonb, date, time, timestamp, timestamptz, bytea (base64), \
     inet, cidr, xml, text[], int4[], int8[], bool[], float8[], uuid[], numeric[], null";

fn typed_param(index: usize, ty: &str, value: &Value) -> Result<PgValue, String> {
    if value.is_null() || ty.eq_ignore_ascii_case("null") {
        return Ok(PgValue::Null);
    }
    let err = |what: &str| format!("param {index}: {what} (type {ty:?}, value {value})");
    let ty_l = ty.trim().to_ascii_lowercase();
    let out = match ty_l.as_str() {
        "int2" | "smallint" => PgValue::Int2(
            as_int(value)
                .and_then(|i| i16::try_from(i).ok())
                .ok_or_else(|| err("expected an int2"))?,
        ),
        "int4" | "int" | "integer" => PgValue::Int4(
            as_int(value)
                .and_then(|i| i32::try_from(i).ok())
                .ok_or_else(|| err("expected an int4"))?,
        ),
        "int8" | "bigint" => PgValue::Int8(as_int(value).ok_or_else(|| err("expected an int8"))?),
        "float4" | "real" => PgValue::Float4(encode_f32(
            as_float(value).ok_or_else(|| err("expected a number"))? as f32,
        )),
        "float8" | "double" | "double precision" => PgValue::Float8(encode_f64(
            as_float(value).ok_or_else(|| err("expected a number"))?,
        )),
        "numeric" | "decimal" | "money" => PgValue::Numeric(
            numeric_text(value).ok_or_else(|| err("expected a number or numeric string"))?,
        ),
        "text" | "varchar" | "char" | "name" | "string" => {
            PgValue::Text(as_text(value).ok_or_else(|| err("expected a string"))?)
        }
        "bool" | "boolean" => {
            PgValue::Bool(as_bool(value).ok_or_else(|| err("expected a boolean"))?)
        }
        "uuid" => PgValue::Uuid(
            as_text(value)
                .filter(|s| is_uuid(s))
                .ok_or_else(|| err("expected a canonical uuid string"))?,
        ),
        "json" => PgValue::Json(json_text(value)),
        "jsonb" => PgValue::Jsonb(json_text(value)),
        "date" => PgValue::Date(
            parse_date(&as_text(value).ok_or_else(|| err("expected YYYY-MM-DD"))?)
                .ok_or_else(|| err("expected YYYY-MM-DD"))?,
        ),
        "time" => PgValue::Time(
            parse_time(&as_text(value).ok_or_else(|| err("expected HH:MM[:SS[.ffffff]]"))?)
                .map(|(t, _)| t)
                .ok_or_else(|| err("expected HH:MM[:SS[.ffffff]]"))?,
        ),
        "timestamp" => PgValue::Timestamp(
            parse_timestamp(&as_text(value).ok_or_else(|| err("expected an ISO-8601 timestamp"))?)
                .map(|(ts, _)| ts)
                .ok_or_else(|| err("expected an ISO-8601 timestamp"))?,
        ),
        "timestamptz" | "timestamp with time zone" => {
            let text =
                as_text(value).ok_or_else(|| err("expected an ISO-8601 timestamp with zone"))?;
            let (ts, offset) = parse_timestamp(&text)
                .ok_or_else(|| err("expected an ISO-8601 timestamp with zone"))?;
            PgValue::TimestampTz(to_utc(ts, offset.unwrap_or(0)))
        }
        "bytea" => PgValue::Bytea(
            base64::engine::general_purpose::STANDARD
                .decode(as_text(value).ok_or_else(|| err("expected base64"))?)
                .map_err(|_| err("expected base64"))?,
        ),
        "inet" => PgValue::Inet(as_text(value).ok_or_else(|| err("expected an IP address"))?),
        "cidr" => PgValue::Cidr(as_text(value).ok_or_else(|| err("expected a CIDR"))?),
        "xml" => PgValue::Xml(as_text(value).ok_or_else(|| err("expected a string"))?),
        "text[]" | "varchar[]" => PgValue::TextArray(
            as_array(value)
                .and_then(|a| a.iter().map(as_text).collect())
                .ok_or_else(|| err("expected an array of strings"))?,
        ),
        "int4[]" | "int[]" | "integer[]" => PgValue::Int4Array(
            as_array(value)
                .and_then(|a| {
                    a.iter()
                        .map(|v| as_int(v).and_then(|i| i32::try_from(i).ok()))
                        .collect()
                })
                .ok_or_else(|| err("expected an array of int4"))?,
        ),
        "int8[]" | "bigint[]" => PgValue::Int8Array(
            as_array(value)
                .and_then(|a| a.iter().map(as_int).collect())
                .ok_or_else(|| err("expected an array of int8"))?,
        ),
        "bool[]" | "boolean[]" => PgValue::BoolArray(
            as_array(value)
                .and_then(|a| a.iter().map(as_bool).collect())
                .ok_or_else(|| err("expected an array of booleans"))?,
        ),
        "float8[]" | "double[]" => PgValue::Float8Array(
            as_array(value)
                .and_then(|a| a.iter().map(|v| as_float(v).map(encode_f64)).collect())
                .ok_or_else(|| err("expected an array of numbers"))?,
        ),
        "uuid[]" => PgValue::UuidArray(
            as_array(value)
                .and_then(|a| {
                    a.iter()
                        .map(|v| as_text(v).filter(|s| is_uuid(s)))
                        .collect()
                })
                .ok_or_else(|| err("expected an array of uuids"))?,
        ),
        "numeric[]" | "decimal[]" => PgValue::NumericArray(
            as_array(value)
                .and_then(|a| a.iter().map(numeric_text).collect())
                .ok_or_else(|| err("expected an array of numerics"))?,
        ),
        _ => {
            return Err(format!(
                "param {index}: unknown parameter type {ty:?}; known types: {TYPED_PARAM_TYPES}"
            ))
        }
    };
    Ok(out)
}

/// Bare JSON arrays: typed by their (homogeneous) content.
fn auto_array(index: usize, items: &[Value]) -> Result<PgValue, String> {
    if items.is_empty() {
        return Ok(PgValue::TextArray(Vec::new()));
    }
    if let Some(strings) = items.iter().map(as_text).collect::<Option<Vec<_>>>() {
        return Ok(PgValue::TextArray(strings));
    }
    if let Some(ints) = items.iter().map(as_int).collect::<Option<Vec<_>>>() {
        return Ok(PgValue::Int8Array(ints));
    }
    if let Some(bools) = items.iter().map(as_bool).collect::<Option<Vec<_>>>() {
        return Ok(PgValue::BoolArray(bools));
    }
    if let Some(floats) = items
        .iter()
        .map(|v| as_float(v).map(encode_f64))
        .collect::<Option<Vec<_>>>()
    {
        return Ok(PgValue::Float8Array(floats));
    }
    Err(format!(
        "param {index}: a bare JSON array must be all strings, all integers, all booleans or all \
         numbers; otherwise pass {{\"type\": \"jsonb\", \"value\": [...]}} or a typed array"
    ))
}

fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "on" | "1" => Some(true),
            "false" | "f" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn as_array(value: &Value) -> Option<&Vec<Value>> {
    value.as_array()
}

/// A numeric literal as text, validated to the digits Postgres accepts.
fn numeric_text(value: &Value) -> Option<String> {
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.trim().to_owned(),
        _ => return None,
    };
    let body = text.strip_prefix(['+', '-']).unwrap_or(&text);
    let valid = !body.is_empty()
        && body.chars().any(|c| c.is_ascii_digit())
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    valid.then_some(text)
}

fn json_text(value: &Value) -> String {
    match value {
        // A string is taken as already-serialized JSON when it parses.
        Value::String(s) if serde_json::from_str::<Value>(s).is_ok() => s.clone(),
        other => other.to_string(),
    }
}

fn is_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

// ── ISO-8601 parsing (dates, times, timestamps, offsets) ────────────────────

fn parse_date(s: &str) -> Option<Date> {
    let s = s.trim();
    match s {
        "infinity" => return Some(Date::PositiveInfinity),
        "-infinity" => return Some(Date::NegativeInfinity),
        _ => {}
    }
    let (year_s, rest) = s.split_once('-')?;
    let (month_s, day_s) = rest.split_once('-')?;
    let year: i32 = year_s.parse().ok()?;
    let month: u32 = month_s.parse().ok()?;
    let day: u32 = day_s.parse().ok()?;
    let valid = (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month);
    valid.then_some(Date::Ymd((year, month, day)))
}

/// `HH:MM[:SS[.ffffff]]` followed by an optional zone; returns the time and
/// the zone offset in seconds when present.
fn parse_time(s: &str) -> Option<(Time, Option<i32>)> {
    let s = s.trim();
    let zone_at = s
        .find(['Z', 'z', '+'])
        .or_else(|| s.rfind('-').filter(|i| *i >= 5));
    let (clock, zone) = match zone_at {
        Some(i) => (&s[..i], Some(&s[i..])),
        None => (s, None),
    };
    let mut parts = clock.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let min: u32 = parts.next()?.parse().ok()?;
    let (sec, micro) = match parts.next() {
        Some(sec_s) => {
            let (whole, frac) = sec_s.split_once('.').unwrap_or((sec_s, ""));
            let sec: u32 = whole.parse().ok()?;
            let micro = if frac.is_empty() {
                0
            } else {
                // Digits only, so the byte length is the digit count and the
                // slice below is on a char boundary.
                if !frac.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let digits = frac.get(..frac.len().min(6))?;
                let scale = 10u32.pow(6 - digits.len() as u32);
                digits.parse::<u32>().ok()? * scale
            };
            (sec, micro)
        }
        None => (0, 0),
    };
    if parts.next().is_some() || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let offset = match zone {
        None => None,
        Some("Z") | Some("z") => Some(0),
        Some(z) => Some(parse_offset(z)?),
    };
    Some((
        Time {
            hour,
            min,
            sec,
            micro,
        },
        offset,
    ))
}

/// `+HH`, `+HH:MM` or `+HHMM` (and `-`). Only ASCII digits are meaningful
/// here, and refusing anything else up front keeps every slice below on a
/// char boundary — client text is never cut by byte length.
fn parse_offset(z: &str) -> Option<i32> {
    if !z.is_ascii() {
        return None;
    }
    let (sign, body) = match z.as_bytes().first()? {
        b'+' => (1, z.get(1..)?),
        b'-' => (-1, z.get(1..)?),
        _ => return None,
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None if body.len() == 4 => (body.get(..2)?, body.get(2..)?),
        None => (body, "0"),
    };
    let two_digits =
        |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !two_digits(h) || !two_digits(m) {
        return None;
    }
    let hours: i32 = h.parse().ok()?;
    let minutes: i32 = m.parse().ok()?;
    (hours <= 23 && minutes <= 59).then_some(sign * (hours * 3600 + minutes * 60))
}

/// `YYYY-MM-DD[T ]HH:MM[:SS[.ffffff]][Z|±HH[:MM]]` or a bare date.
fn parse_timestamp(s: &str) -> Option<(Timestamp, Option<i32>)> {
    let s = s.trim();
    match s {
        "infinity" => {
            return Some((
                Timestamp {
                    date: Date::PositiveInfinity,
                    time: Time {
                        hour: 0,
                        min: 0,
                        sec: 0,
                        micro: 0,
                    },
                },
                None,
            ))
        }
        "-infinity" => {
            return Some((
                Timestamp {
                    date: Date::NegativeInfinity,
                    time: Time {
                        hour: 0,
                        min: 0,
                        sec: 0,
                        micro: 0,
                    },
                },
                None,
            ))
        }
        _ => {}
    }
    let (date_s, time_s) = match s.find(['T', 't', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let date = parse_date(date_s)?;
    let (time, offset) = match time_s {
        Some(t) if !t.is_empty() => parse_time(t)?,
        _ => (
            Time {
                hour: 0,
                min: 0,
                sec: 0,
                micro: 0,
            },
            None,
        ),
    };
    Some((Timestamp { date, time }, offset))
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`] (`civil_from_days`).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// Shifts a local timestamp by `offset_secs` (east-positive) to UTC and wraps
/// it as the host's `timestamp-tz` (which carries UTC with a zero offset).
fn to_utc(ts: Timestamp, offset_secs: i32) -> TimestampTz {
    let Date::Ymd((y, m, d)) = ts.date else {
        return TimestampTz {
            timestamp: ts,
            offset: Offset::WesternHemisphereSecs(0),
        };
    };
    let local_secs = days_from_civil(y, m, d) * 86_400
        + i64::from(ts.time.hour) * 3600
        + i64::from(ts.time.min) * 60
        + i64::from(ts.time.sec);
    let utc_secs = local_secs - i64::from(offset_secs);
    let days = utc_secs.div_euclid(86_400);
    let rem = utc_secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    TimestampTz {
        timestamp: Timestamp {
            date: Date::Ymd((y, m, d)),
            time: Time {
                hour: rem / 3600,
                min: (rem % 3600) / 60,
                sec: rem % 60,
                micro: ts.time.micro,
            },
        },
        offset: Offset::WesternHemisphereSecs(0),
    }
}
