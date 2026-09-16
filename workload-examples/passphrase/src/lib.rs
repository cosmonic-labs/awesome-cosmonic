//! Password Generator — a pure-compute HTTP component for Cosmonic Desktop.
//!
//! Generates passwords from the host's cryptographic RNG (`wasi:random`) and
//! reports the entropy of each one, so the number on screen is a fact about the
//! generator rather than a vibe about the characters.
//!
//! It declares NO outbound network access. That is the interesting part: a
//! password generator is exactly the kind of tool where "this cannot phone
//! home" is the property you want, and here it is enforced by the host rather
//! than promised by the page. Its Launchpad card reads `OUTBOUND none`.
//!
//! Routes:
//!   GET /                       the browser UI
//!   GET /api?…                  JSON: { passwords: [...], entropyBits, alphabet }
//!   GET /healthz                "ok"
//!
//! Query parameters (all optional): `length` (4–128, default 20), `count`
//! (1–20, default 5), and the four class toggles `lower`, `upper`, `digits`,
//! `symbols` (`1`/`0`, default all on except symbols which is on too), plus
//! `avoidAmbiguous=1` to drop characters that are hard to tell apart.

use wasip3::http::types::{ErrorCode, Fields, Request, Response};
use wasip3::http_compat::BodyWriter;

struct Component;

wasip3::http::service::export!(Component);

const LOWER: &str = "abcdefghijkmnopqrstuvwxyz"; // no l
const UPPER: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ"; // no I, O
const DIGITS: &str = "23456789"; // no 0, 1
const LOWER_ALL: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER_ALL: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS_ALL: &str = "0123456789";
const SYMBOLS: &str = "!@#$%^&*()-_=+[]{};:,.?";

const MIN_LEN: usize = 4;
const MAX_LEN: usize = 128;
const MAX_COUNT: usize = 20;

/// Uniform index in `0..n`, by rejection sampling.
///
/// `% n` on a random byte is the obvious version and it is biased whenever `n`
/// does not divide 256 — for a 70-character alphabet the first 46 characters
/// come up ~1.4% more often than the rest. For a password generator that is a
/// real (if small) loss of entropy, and it costs nothing to avoid: draw a byte,
/// throw it away if it lands in the short tail, draw again.
fn uniform(n: usize) -> usize {
    debug_assert!(n > 0 && n <= 256);
    let limit = 256 - (256 % n); // largest multiple of n that fits in a byte
    loop {
        // A small batch: one host call per byte would be a syscall per character.
        let bytes = wasip3::random::random::get_random_bytes(32);
        for b in bytes {
            let v = b as usize;
            if v < limit {
                return v % n;
            }
        }
    }
}

/// Shuffle in place (Fisher–Yates), so the guaranteed one-per-class characters
/// don't always sit at the front.
fn shuffle(v: &mut [char]) {
    if v.len() < 2 {
        return;
    }
    for i in (1..v.len()).rev() {
        v.swap(i, uniform(i + 1));
    }
}

struct Opts {
    length: usize,
    count: usize,
    classes: Vec<&'static str>,
    avoid_ambiguous: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self { length: 20, count: 5, classes: vec![], avoid_ambiguous: true }
    }
}

fn clamp(v: usize, lo: usize, hi: usize) -> usize {
    v.max(lo).min(hi)
}

/// `a=1&b=hello` → the value for `key`. Tiny and allocation-light; the query
/// strings this accepts are a handful of short scalars.
fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn flag(query: &str, key: &str, default: bool) -> bool {
    match param(query, key) {
        None => default,
        Some(v) => !matches!(v, "0" | "false" | "no" | ""),
    }
}

fn parse_opts(query: &str) -> Opts {
    let d = Opts::default();
    let avoid_ambiguous = flag(query, "avoidAmbiguous", d.avoid_ambiguous);
    let (lower, upper, digits) = if avoid_ambiguous {
        (LOWER, UPPER, DIGITS)
    } else {
        (LOWER_ALL, UPPER_ALL, DIGITS_ALL)
    };
    let mut classes: Vec<&'static str> = Vec::new();
    if flag(query, "lower", true) {
        classes.push(lower);
    }
    if flag(query, "upper", true) {
        classes.push(upper);
    }
    if flag(query, "digits", true) {
        classes.push(digits);
    }
    if flag(query, "symbols", true) {
        classes.push(SYMBOLS);
    }
    // Every class turned off would mean an empty alphabet and an empty
    // password, which is a worse answer than quietly keeping one.
    if classes.is_empty() {
        classes.push(lower);
    }
    Opts {
        length: clamp(param(query, "length").and_then(|v| v.parse().ok()).unwrap_or(d.length), MIN_LEN, MAX_LEN),
        count: clamp(param(query, "count").and_then(|v| v.parse().ok()).unwrap_or(d.count), 1, MAX_COUNT),
        classes,
        avoid_ambiguous,
    }
}

fn generate(opts: &Opts) -> Vec<String> {
    let alphabet: Vec<char> = opts.classes.iter().flat_map(|c| c.chars()).collect();
    (0..opts.count)
        .map(|_| {
            let mut out: Vec<char> = Vec::with_capacity(opts.length);
            // One character from each requested class first, so "include
            // digits" means the password actually contains one rather than
            // probably containing one.
            for class in &opts.classes {
                if out.len() == opts.length {
                    break;
                }
                let chars: Vec<char> = class.chars().collect();
                out.push(chars[uniform(chars.len())]);
            }
            while out.len() < opts.length {
                out.push(alphabet[uniform(alphabet.len())]);
            }
            shuffle(&mut out);
            out.into_iter().collect()
        })
        .collect()
}

/// Entropy of the passwords this generator actually produces, in bits.
///
/// NOT `log2(alphabet) * length`. That is the entropy of a free choice at every
/// position, and `generate` does not make one: it plants a character from each
/// enabled class first, so strings missing a class are never produced. Counting
/// the smaller space matters most exactly where a user is most exposed, at short
/// lengths: for a 4-character password over 4 classes the free-choice figure
/// overstates by ~4 bits, a guess space 15x larger than the real one. At the
/// default length the two agree to within a third of a bit, but a number on a
/// password tool should be right at both ends.
///
/// Counted by inclusion-exclusion over the enabled classes: start from every
/// string, subtract those missing class A, missing B, ... add back those missing
/// two, and so on. At most four classes, so at most sixteen terms.
fn entropy_bits(class_sizes: &[usize], length: usize) -> f64 {
    let alphabet_len: usize = class_sizes.iter().sum();
    if alphabet_len == 0 || length == 0 {
        return 0.0;
    }
    // A password shorter than the class count cannot hold one of each; generate
    // stops planting when it runs out of room, so the space is the free one.
    if length < class_sizes.len() {
        return (alphabet_len as f64).log2() * length as f64;
    }
    let k = class_sizes.len();
    let mut total = 0f64;
    for mask in 0u32..(1u32 << k) {
        let excluded: usize = (0..k)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| class_sizes[i])
            .sum();
        let remaining = alphabet_len - excluded;
        let term = (remaining as f64).powi(length as i32);
        if (mask.count_ones() % 2) == 0 {
            total += term;
        } else {
            total -= term;
        }
    }
    if total <= 0.0 {
        return 0.0;
    }
    total.log2()
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn api_json(opts: &Opts) -> String {
    let class_sizes: Vec<usize> = opts.classes.iter().map(|c| c.chars().count()).collect();
    let alphabet_len: usize = class_sizes.iter().sum();
    let pws = generate(opts);
    let list = pws
        .iter()
        .map(|p| format!("\"{}\"", json_escape(p)))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"passwords\":[{}],\"length\":{},\"count\":{},\"alphabetSize\":{},\"entropyBits\":{:.1},\"avoidAmbiguous\":{}}}",
        list,
        opts.length,
        opts.count,
        alphabet_len,
        entropy_bits(&class_sizes, opts.length),
        opts.avoid_ambiguous
    )
}

const PAGE: &str = include_str!("index.html");

fn respond(status: u16, content_type: &str, body: String) -> Result<Response, ErrorCode> {
    let headers = Fields::from_list(&[
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        // Generated secrets must not sit in a shared cache.
        ("cache-control".to_string(), b"no-store".to_vec()),
        ("x-content-type-options".to_string(), b"nosniff".to_vec()),
    ])
    .map_err(|err| ErrorCode::InternalError(Some(format!("invalid headers: {err}"))))?;

    let (mut writer, body_rx, result_rx) = BodyWriter::new();
    let (response, _transmit) = Response::new(headers, Some(body_rx), result_rx);
    response
        .set_status_code(status)
        .map_err(|()| ErrorCode::InternalError(Some("invalid status code".into())))?;

    wasip3::wit_bindgen::spawn(async move {
        let frame = http_body::Frame::data(bytes::Bytes::from(body));
        let _ = writer.send_frame(frame).await;
        drop(writer.stream_writer);
        let _ = writer.result_writer.write(Ok(None)).await;
    });

    Ok(response)
}

impl wasip3::exports::http::handler::Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let path_with_query = request.get_path_with_query().unwrap_or_else(|| "/".to_string());
        let (path, query) = match path_with_query.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path_with_query.as_str(), ""),
        };

        match path {
            "/healthz" => respond(200, "text/plain; charset=utf-8", "ok\n".to_string()),
            "/api" => respond(200, "application/json; charset=utf-8", api_json(&parse_opts(query))),
            "/" | "" => respond(200, "text/html; charset=utf-8", PAGE.to_string()),
            _ => respond(404, "text/plain; charset=utf-8", "not found\n".to_string()),
        }
    }
}
