//! AWS Signature Version 4, computed inside the component.
//!
//! No AWS SDK is involved: the canonical request, string-to-sign and
//! signing-key derivation follow the published algorithm
//! (<https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>)
//! using `hmac` + `sha2`. [`self_test`] reproduces the four S3 example
//! signatures AWS publishes, so a signer regression is detectable offline.
//!
//! Rules that matter (each one has bitten a hand-rolled signer before):
//!
//! - **URI encoding** uses the AWS `UriEncode` set: only `A-Z a-z 0-9 - _ . ~`
//!   pass through, everything else is `%XX` with uppercase hex; `/` is kept
//!   in paths and encoded in query keys/values.
//! - **Canonical URI**: S3 signs the raw (single-encoded) path; every other
//!   service double-encodes it (a Lambda ARN colon is `%3A` on the wire and
//!   `%253A` in the canonical request).
//! - **Canonical query**: `key=value` pairs, both URI-encoded, sorted by key
//!   then value, joined with `&`; a valueless key becomes `key=`.
//! - **Canonical headers**: lowercase names, values trimmed with runs of
//!   spaces collapsed, sorted by name; `host` is always signed, and every
//!   `x-amz-*` header that is sent must be signed.
//! - **Payload hash** is the hex SHA-256 of the exact bytes sent (the empty
//!   body hashes to `e3b0c442…b855`). S3 additionally requires it in the
//!   signed `x-amz-content-sha256` header; `UNSIGNED-PAYLOAD` is never used.
//! - The credential scope date is the UTC date of `x-amz-date`.

use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The algorithm token in the credential scope and `Authorization` header.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Hex SHA-256 of the empty payload.
pub const EMPTY_PAYLOAD_HASH: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Static credentials: an access key pair plus the session token that
/// temporary (`ASIA…`) keys carry.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// Never prints the secret or the token, so a stray `{:?}` in a log
    /// cannot leak them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("session_token", &self.session_token.is_some())
            .finish_non_exhaustive()
    }
}

/// Everything the signer needs to know about one request.
pub struct SigningRequest<'a> {
    /// HTTP method, uppercase.
    pub method: &'a str,
    /// The `Host` header value the client will send (authority, including a
    /// non-default port).
    pub host: &'a str,
    /// The path exactly as it goes on the wire (already URI-encoded per
    /// segment, `/` kept). Empty means `/`.
    pub raw_path: &'a str,
    /// The canonical (sorted, encoded) query string, also sent verbatim.
    pub canonical_query: &'a str,
    /// Headers to sign besides `host`: `(name, value)` in any case. Every
    /// header listed here must be sent with exactly this value.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the request body.
    pub payload_hash: &'a str,
    /// Credential-scope service name (`s3`, `sts`, `ec2`, `lambda`, `logs`).
    pub service: &'a str,
    /// Credential-scope region.
    pub region: &'a str,
    /// `YYYYMMDD'T'HHMMSS'Z'` — the value of the `x-amz-date` header.
    pub amz_date: &'a str,
    /// `true` for S3 (single-encoded canonical URI), `false` otherwise.
    pub s3_style: bool,
}

/// A computed signature with the intermediate strings, kept so a mismatch
/// against AWS's echoed `CanonicalRequest` can be diagnosed.
#[derive(Debug, Clone)]
pub struct Signature {
    pub authorization: String,
    pub signed_headers: String,
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
}

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so `new_from_slice` cannot fail; the
    // fallback exists only so that no code path can panic.
    let mut mac =
        HmacSha256::new_from_slice(key).unwrap_or_else(|_| HmacSha256::new(&Default::default()));
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// AWS `UriEncode`: unreserved characters pass, everything else is `%XX`
/// (uppercase). `/` is encoded unless `encode_slash` is false.
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len().saturating_mul(3));
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                // Writing to a String cannot fail.
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// Encodes an object key or path for the wire: each `/`-separated segment is
/// URI-encoded on its own, so `a b/c+d` becomes `a%20b/c%2Bd` and the slashes
/// keep their meaning.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| uri_encode(segment, true))
        .collect::<Vec<_>>()
        .join("/")
}

/// Builds the canonical query string from raw `(key, value)` pairs: both
/// sides URI-encoded, sorted by key then value. The same string is put on
/// the wire, so the signed and the sent query can never differ.
pub fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| (uri_encode(key, true), uri_encode(value, true)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Canonical header value: trimmed, with internal runs of whitespace
/// collapsed to one space.
fn canonical_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Signs one request. Pure computation — no clock, no I/O.
pub fn sign(request: &SigningRequest<'_>, credentials: &Credentials) -> Signature {
    // 1. Canonical headers: host plus everything the caller listed.
    let mut headers: Vec<(String, String)> = Vec::with_capacity(request.headers.len() + 1);
    headers.push(("host".to_owned(), canonical_header_value(request.host)));
    for (name, value) in request.headers {
        headers.push((name.to_ascii_lowercase(), canonical_header_value(value)));
    }
    headers.sort();
    headers.dedup_by(|later, earlier| {
        if later.0 == earlier.0 {
            // Repeated header: AWS joins the values with commas.
            earlier.1.push(',');
            earlier.1.push_str(&later.1);
            true
        } else {
            false
        }
    });
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical_headers = String::new();
    for (name, value) in &headers {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value);
        canonical_headers.push('\n');
    }

    // 2. Canonical URI.
    let raw_path = if request.raw_path.is_empty() {
        "/"
    } else {
        request.raw_path
    };
    let canonical_uri = if request.s3_style {
        raw_path.to_owned()
    } else {
        uri_encode(raw_path, false)
    };

    // 3. Canonical request and string to sign.
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        canonical_uri,
        request.canonical_query,
        canonical_headers,
        signed_headers,
        request.payload_hash
    );
    let date_stamp = request.amz_date.get(..8).unwrap_or("");
    let scope = format!(
        "{date_stamp}/{}/{}/aws4_request",
        request.region, request.service
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        request.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );

    // 4. Signing key and signature.
    let k_secret = format!("AWS4{}", credentials.secret_access_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, request.region.as_bytes());
    let k_service = hmac_sha256(&k_region, request.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    Signature {
        authorization,
        signed_headers,
        canonical_request,
        string_to_sign,
        signature,
    }
}

/// One published example, signed with the documented example credentials.
#[derive(Debug, Clone)]
pub struct SelfTestCase {
    pub name: &'static str,
    pub expected: &'static str,
    pub computed: String,
    pub pass: bool,
}

/// The access key of the AWS documentation examples.
pub const EXAMPLE_ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
/// The secret key of the AWS documentation examples.
pub const EXAMPLE_SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
/// The timestamp of the AWS documentation examples.
pub const EXAMPLE_DATE: &str = "20130524T000000Z";

/// Signs the four S3 examples from "Authenticating Requests: Using the
/// Authorization Header (AWS Signature Version 4)" and compares each with
/// the signature AWS prints there. All four must pass; a failure means the
/// signer (not the user's secret) is wrong.
pub fn self_test() -> Vec<SelfTestCase> {
    let credentials = Credentials {
        access_key_id: EXAMPLE_ACCESS_KEY.to_owned(),
        secret_access_key: EXAMPLE_SECRET_KEY.to_owned(),
        session_token: None,
    };
    let host = "examplebucket.s3.amazonaws.com";
    let date = ("x-amz-date".to_owned(), EXAMPLE_DATE.to_owned());
    let empty = (
        "x-amz-content-sha256".to_owned(),
        EMPTY_PAYLOAD_HASH.to_owned(),
    );
    let put_body = b"Welcome to Amazon S3.";
    let put_hash = sha256_hex(put_body);

    let get_headers = vec![
        date.clone(),
        ("Range".to_owned(), "bytes=0-9".to_owned()),
        empty.clone(),
    ];
    let put_headers = vec![
        date.clone(),
        (
            "Date".to_owned(),
            "Fri, 24 May 2013 00:00:00 GMT".to_owned(),
        ),
        (
            "x-amz-storage-class".to_owned(),
            "REDUCED_REDUNDANCY".to_owned(),
        ),
        ("x-amz-content-sha256".to_owned(), put_hash.clone()),
    ];
    let plain_headers = vec![date, empty];

    struct Vector<'a> {
        name: &'static str,
        method: &'static str,
        path: &'static str,
        query: &'static str,
        headers: &'a [(String, String)],
        payload_hash: &'a str,
        expected: &'static str,
    }
    let cases = [
        Vector {
            name: "get-object-range",
            method: "GET",
            path: "/test.txt",
            query: "",
            headers: &get_headers,
            payload_hash: EMPTY_PAYLOAD_HASH,
            expected: "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        },
        Vector {
            name: "put-object",
            method: "PUT",
            path: "/test%24file.text",
            query: "",
            headers: &put_headers,
            payload_hash: put_hash.as_str(),
            expected: "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
        },
        Vector {
            name: "get-bucket-lifecycle",
            method: "GET",
            path: "/",
            query: "lifecycle=",
            headers: &plain_headers,
            payload_hash: EMPTY_PAYLOAD_HASH,
            expected: "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
        },
        Vector {
            name: "list-objects",
            method: "GET",
            path: "/",
            query: "max-keys=2&prefix=J",
            headers: &plain_headers,
            payload_hash: EMPTY_PAYLOAD_HASH,
            expected: "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
        },
    ];

    cases
        .iter()
        .map(|case| {
            let signed = sign(
                &SigningRequest {
                    method: case.method,
                    host,
                    raw_path: case.path,
                    canonical_query: case.query,
                    headers: case.headers,
                    payload_hash: case.payload_hash,
                    service: "s3",
                    region: "us-east-1",
                    amz_date: EXAMPLE_DATE,
                    s3_style: true,
                },
                &credentials,
            );
            let pass = signed.signature == case.expected;
            SelfTestCase {
                name: case.name,
                expected: case.expected,
                computed: signed.signature,
                pass,
            }
        })
        .collect()
}

pub mod time {
    //! Civil-date arithmetic for `x-amz-date`, ISO-8601 output and the two
    //! date formats AWS sends back (RFC 3339 in bodies, IMF-fixdate in the
    //! `Date` header). Hand-written (Howard Hinnant's algorithms) so no
    //! `chrono`/`time` dependency is needed; every function tolerates
    //! garbage input by returning `None` rather than panicking.

    /// Seconds since the Unix epoch from the host wall clock (0 if the clock
    /// is before the epoch, which no real host reports).
    pub fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    /// Milliseconds since the Unix epoch from the host wall clock.
    pub fn now_millis() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }

    /// Days since 1970-01-01 for a proleptic Gregorian civil date.
    pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let m = i64::from(month);
        let d = i64::from(day);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// Civil date `(year, month, day)` for days since 1970-01-01.
    pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        // `d` is 1..=31 and `m` 1..=12 by construction.
        (year, m as u32, d as u32)
    }

    /// Splits epoch seconds into `(year, month, day, hour, minute, second)`.
    pub fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        (
            y,
            m,
            d,
            (sod / 3600) as u32,
            ((sod % 3600) / 60) as u32,
            (sod % 60) as u32,
        )
    }

    /// `YYYYMMDD'T'HHMMSS'Z'` — the `x-amz-date` format.
    pub fn format_amz_date(secs: i64) -> String {
        let (y, mo, d, h, mi, s) = civil(secs);
        format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
    }

    /// `YYYY-MM-DDTHH:MM:SS.mmmZ` for an epoch-milliseconds value.
    pub fn format_iso_millis(millis: i64) -> String {
        let secs = millis.div_euclid(1000);
        let frac = millis.rem_euclid(1000);
        let (y, mo, d, h, mi, s) = civil(secs);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{frac:03}Z")
    }

    fn parse_num(s: &str) -> Option<i64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }

    /// Parses `YYYY-MM-DD[Tt ]HH:MM[:SS[.fff]](Z|±HH:MM)` or a bare
    /// `YYYY-MM-DD` into epoch milliseconds.
    pub fn parse_rfc3339(input: &str) -> Option<i64> {
        let s = input.trim();
        // Every valid RFC 3339 string is ASCII. Checking that first also makes
        // the byte-10 split below a guaranteed char boundary: `split_at` on a
        // string whose first ten bytes hold a multi-byte character would panic
        // (and trap the instance) on this client-supplied input.
        if !s.is_ascii() || s.len() < 10 {
            return None;
        }
        let (date, rest) = s.split_at(10);
        let mut parts = date.split('-');
        let year = parse_num(parts.next()?)?;
        let month = parse_num(parts.next()?)?;
        let day = parse_num(parts.next()?)?;
        if parts.next().is_some()
            || !(0..=9999).contains(&year)
            || !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
        {
            return None;
        }
        let mut secs = days_from_civil(year, month as u32, day as u32) * 86_400;
        let mut millis = 0;
        if !rest.is_empty() {
            let rest = rest.strip_prefix(['T', 't', ' '])?;
            // Split off the zone designator.
            let (clock, zone) = match rest.find(['Z', 'z', '+', '-']) {
                Some(idx) => (&rest[..idx], &rest[idx..]),
                None => (rest, ""),
            };
            let (hms, frac) = match clock.split_once('.') {
                Some((hms, frac)) => (hms, frac),
                None => (clock, ""),
            };
            let mut fields = hms.split(':');
            let hour = parse_num(fields.next()?)?;
            let minute = parse_num(fields.next()?)?;
            let second = match fields.next() {
                Some(v) => parse_num(v)?,
                None => 0,
            };
            if fields.next().is_some() || hour > 23 || minute > 59 || second > 60 {
                return None;
            }
            secs += hour * 3600 + minute * 60 + second;
            if !frac.is_empty() {
                let digits: String = frac.chars().take(3).collect();
                if !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let scale = 10_i64.pow(3 - digits.len() as u32);
                millis = parse_num(&digits)? * scale;
            }
            match zone {
                "" | "Z" | "z" => {}
                _ => {
                    let sign = if zone.starts_with('-') { 1 } else { -1 };
                    let (zh, zm) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
                    let zh = parse_num(zh)?;
                    let zm = parse_num(zm)?;
                    if zh > 23 || zm > 59 {
                        return None;
                    }
                    secs += sign * (zh * 3600 + zm * 60);
                }
            }
        }
        secs.checked_mul(1000)?.checked_add(millis)
    }

    /// Parses an HTTP `Date` header (`Sun, 06 Nov 1994 08:49:37 GMT`) into
    /// epoch seconds.
    pub fn parse_imf_fixdate(input: &str) -> Option<i64> {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let tokens: Vec<&str> = input.split_whitespace().collect();
        // With or without the leading weekday.
        let start = if tokens.len() == 6 { 1 } else { 0 };
        if tokens.len() != start + 5 {
            return None;
        }
        let day = parse_num(tokens[start])?;
        let month = MONTHS
            .iter()
            .position(|m| m.eq_ignore_ascii_case(tokens[start + 1]))? as u32
            + 1;
        let year = parse_num(tokens[start + 2])?;
        let mut hms = tokens[start + 3].split(':');
        let hour = parse_num(hms.next()?)?;
        let minute = parse_num(hms.next()?)?;
        let second = parse_num(hms.next()?)?;
        if !(0..=9999).contains(&year)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        Some(days_from_civil(year, month, day as u32) * 86_400 + hour * 3600 + minute * 60 + second)
    }
}
