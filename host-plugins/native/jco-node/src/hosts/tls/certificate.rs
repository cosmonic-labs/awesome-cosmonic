//! Certificates as Node's `getPeerCertificate()` describes them, and `tls.checkServerIdentity`.

use std::net::IpAddr;

use rustls::pki_types::CertificateDer;
use sha1::Digest as _;
use x509_parser::{
    certificate::X509Certificate,
    extensions::GeneralName,
    objects::{oid_registry, oid2sn},
    prelude::FromDer,
    x509::X509Name,
};

use super::wire::{Value, error_with};

fn fingerprint(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(":")
}

/// A distinguished name as Node's object of short names (`{ CN: "localhost", O: "…" }`).
fn name(name: &X509Name<'_>) -> Value {
    Value::Object(
        name.iter_attributes()
            .filter_map(|attribute| {
                let key = oid2sn(attribute.attr_type(), oid_registry()).ok()?;
                Some((key.to_owned(), Value::Text(attribute.as_str().ok()?.to_owned())))
            })
            .collect(),
    )
}

/// OpenSSL's `ASN1_TIME_print`: `Nov 18 00:00:00 2026 GMT`.
fn time(time: x509_parser::time::ASN1Time) -> String {
    let time = time.to_datetime();
    let month = time.month().to_string();
    format!("{} {:>2} {:02}:{:02}:{:02} {} GMT", &month[..3], time.day(), time.hour(), time.minute(), time.second(), time.year())
}

/// OpenSSL's rendering of an IP address in a subject alternative name.
fn ip(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?).to_string()),
        16 => Some(
            bytes.chunks(2).map(|pair| format!("{:X}", u16::from_be_bytes([pair[0], pair[1]]))).collect::<Vec<_>>().join(":"),
        ),
        _ => None,
    }
}

struct Parsed {
    entries: Vec<(String, Value)>,
    self_issued: bool,
}

fn parse(der: &[u8]) -> Option<Parsed> {
    let (_, certificate) = X509Certificate::from_der(der).ok()?;
    let mut entries = vec![
        ("subject".to_owned(), name(certificate.subject())),
        ("issuer".to_owned(), name(certificate.issuer())),
    ];
    if let Ok(Some(names)) = certificate.subject_alternative_name() {
        let names: Vec<String> = names
            .value
            .general_names
            .iter()
            .filter_map(|general| match general {
                GeneralName::DNSName(dns) => Some(format!("DNS:{dns}")),
                GeneralName::IPAddress(bytes) => ip(bytes).map(|address| format!("IP Address:{address}")),
                GeneralName::RFC822Name(email) => Some(format!("email:{email}")),
                GeneralName::URI(uri) => Some(format!("URI:{uri}")),
                _ => None,
            })
            .collect();
        entries.push(("subjectaltname".to_owned(), Value::Text(names.join(", "))));
    }
    let serial = certificate.tbs_certificate.raw_serial();
    let serial = serial.iter().skip_while(|byte| **byte == 0).map(|byte| format!("{byte:02X}")).collect::<String>();
    entries.extend([
        ("ca".to_owned(), Value::Bool(certificate.is_ca())),
        ("pubkey".to_owned(), Value::Bytes(certificate.public_key().raw.to_vec())),
        ("valid_from".to_owned(), Value::Text(time(certificate.validity().not_before))),
        ("valid_to".to_owned(), Value::Text(time(certificate.validity().not_after))),
        ("serialNumber".to_owned(), Value::Text(if serial.is_empty() { "00".into() } else { serial })),
        ("fingerprint".to_owned(), Value::Text(fingerprint(&sha1::Sha1::digest(der)))),
        ("fingerprint256".to_owned(), Value::Text(fingerprint(&sha2::Sha256::digest(der)))),
        ("fingerprint512".to_owned(), Value::Text(fingerprint(&sha2::Sha512::digest(der)))),
        ("raw".to_owned(), Value::Bytes(der.to_vec())),
    ]);
    Some(Parsed { entries, self_issued: certificate.subject().as_raw() == certificate.issuer().as_raw() })
}

/// The first certificate of `chain` as `getPeerCertificate(detailed)` returns it: `{}` with no
/// certificate; with `detailed`, each certificate's `issuerCertificate` is the next one in the
/// chain, and a self-signed certificate's is itself.
pub(crate) fn describe(chain: &[CertificateDer<'_>], detailed: bool) -> Value {
    fn build(chain: &[CertificateDer<'_>], detailed: bool) -> Value {
        let Some(parsed) = chain.first().and_then(|der| parse(der)) else { return Value::Object(Vec::new()) };
        let mut entries = parsed.entries;
        if detailed {
            if parsed.self_issued {
                entries.push(("issuerCertificate".to_owned(), Value::Parent));
            } else if chain.len() > 1 {
                entries.push(("issuerCertificate".to_owned(), build(&chain[1..], detailed)));
            }
        }
        Value::Object(entries)
    }
    build(chain, detailed)
}

/// Node's wildcard match of one name against a certificate's: labels compared case-insensitively,
/// a `*` allowed only within the first of at least three labels.
pub(crate) fn name_matches(host: &[&str], pattern: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let pattern = pattern.strip_suffix('.').unwrap_or(&pattern);
    let parts: Vec<&str> = pattern.split('.').collect();
    if parts.len() != host.len() || parts.iter().any(|part| part.is_empty()) {
        return false;
    }
    if parts[1..] != host[1..] {
        return false;
    }
    let (first, label) = (parts[0], host[0]);
    if first == label {
        return true;
    }
    match first.split_once('*') {
        Some((prefix, suffix)) if !suffix.contains('*') && parts.len() > 2 && !first.starts_with("xn--") => {
            label.len() >= prefix.len() + suffix.len() && label.starts_with(prefix) && label.ends_with(suffix)
        }
        _ => false,
    }
}

/// `tls.checkServerIdentity(hostname, cert)`: `undefined` if `cert` (as `describe` returns it)
/// names `hostname`, else Node's `ERR_TLS_CERT_ALTNAME_INVALID` error.
pub(crate) fn check_identity(hostname: &str, certificate: &Value) -> Value {
    let altnames = certificate.get("subjectaltname").and_then(Value::text).unwrap_or("");
    let entries: Vec<&str> = altnames.split(", ").filter(|entry| !entry.is_empty()).collect();
    let dns: Vec<&str> = entries.iter().filter_map(|entry| entry.strip_prefix("DNS:")).collect();
    let ips: Vec<&str> = entries.iter().filter_map(|entry| entry.strip_prefix("IP Address:")).collect();
    let host = hostname.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    let reason = if let Ok(address) = host.parse::<IpAddr>() {
        let rendered = match address {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => ip(&v6.octets()).unwrap_or_default(),
        };
        (!ips.iter().any(|ip| ip.eq_ignore_ascii_case(&rendered)))
            .then(|| format!("IP: {hostname} is not in the cert's list: {}", ips.join(", ")))
    } else {
        let labels: Vec<&str> = host.split('.').collect();
        let common = certificate.get("subject").and_then(|subject| subject.get("CN")).and_then(Value::text);
        if !dns.is_empty() {
            (!dns.iter().any(|pattern| name_matches(&labels, pattern)))
                .then(|| format!("Host: {hostname}. is not in the cert's altnames: {altnames}"))
        } else if let Some(common) = common {
            (!name_matches(&labels, common)).then(|| format!("Host: {hostname}. is not cert's CN: {common}"))
        } else {
            Some("Cert does not contain a DNS name".to_owned())
        }
    };
    match reason {
        None => Value::Undefined,
        Some(reason) => error_with(
            Some("ERR_TLS_CERT_ALTNAME_INVALID"),
            format!("Hostname/IP does not match certificate's altnames: {reason}"),
            [("reason", reason.into()), ("host", hostname.into())],
        ),
    }
}

/// `certificate` as one PEM block, as `tls.rootCertificates` lists them.
pub(crate) fn pem(der: &[u8]) -> String {
    use base64::Engine as _;
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body.as_bytes().chunks(64).map(|line| std::str::from_utf8(line).unwrap_or_default()).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----", lines.join("\n"))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::tls::wire::object;

    fn certificate(altnames: &str) -> Value {
        object([("subjectaltname", altnames.into())])
    }

    #[test]
    fn identity_follows_nodes_rules() {
        assert_eq!(check_identity("localhost", &certificate("DNS:localhost")), Value::Undefined);
        assert_eq!(check_identity("a.example.com", &certificate("DNS:*.example.com")), Value::Undefined);
        assert_eq!(check_identity("127.0.0.1", &certificate("DNS:localhost, IP Address:127.0.0.1")), Value::Undefined);
        let mismatch = check_identity("wrong.example", &certificate("DNS:localhost"));
        assert_eq!(mismatch.get("code"), Some(&Value::Text("ERR_TLS_CERT_ALTNAME_INVALID".into())));
        assert_eq!(
            mismatch.get("message").and_then(Value::text),
            Some("Hostname/IP does not match certificate's altnames: Host: wrong.example. is not in the cert's altnames: DNS:localhost")
        );
        // A wildcard covers one label, and not a two-label pattern.
        assert_ne!(check_identity("a.b.example.com", &certificate("DNS:*.example.com")), Value::Undefined);
        assert_ne!(check_identity("example.com", &certificate("DNS:*.com")), Value::Undefined);
    }
}
