//! PII detection and redaction — pure compute, no I/O.
//!
//! One entry point, [`redact`], scans a string for the PII categories in
//! [`KNOWN_TYPES`] and replaces every hit with a per-category placeholder
//! (`[REDACTED_EMAIL]`, `[REDACTED_SSN]`, …), returning the rewritten text plus
//! per-category and total counts.
//!
//! ## Matching model
//!
//! Every category is a [`regex`] pattern compiled **once** behind a
//! [`OnceLock`]. The `regex` crate runs in guaranteed linear time with no
//! catastrophic backtracking, so untrusted input cannot trigger a ReDoS blowup.
//!
//! All candidate matches from all enabled categories are gathered, then
//! resolved in a **single left-to-right pass**: candidates are ordered by start
//! offset, then by category precedence, then by length, and accepted greedily
//! so overlapping matches never double-count. Precedence puts `email` first so
//! the digits inside an address are not re-hit as a phone number or card.
//!
//! Two categories carry extra rules:
//! - **credit_card** candidates (13–19 digit runs) are accepted only if they
//!   pass the [Luhn] checksum, which cuts most false positives.
//! - bare 9-digit **us_ssn** is accepted only next to an "SSN" / "social
//!   security" context word; the separated 3-2-4 form (`123-45-6789`,
//!   `123.45.6789`, or `123 45 6789`) is always accepted.
//!
//! [Luhn]: https://en.wikipedia.org/wiki/Luhn_algorithm

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

/// Maximum input size accepted by [`redact`] (256 KiB). Larger inputs return
/// [`RedactError::TooLarge`] rather than being scanned.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// The canonical PII category names, in a stable order. These are the values
/// accepted in the `types` argument and the keys returned in `counts`.
pub const KNOWN_TYPES: &[&str] = &[
    "email",
    "us_ssn",
    "phone",
    "credit_card",
    "ipv4",
    "aws_access_key_id",
];

/// Placeholder substituted for a redacted value of the given category.
fn placeholder(category: &str) -> &'static str {
    match category {
        "email" => "[REDACTED_EMAIL]",
        "us_ssn" => "[REDACTED_SSN]",
        "phone" => "[REDACTED_PHONE]",
        "credit_card" => "[REDACTED_CC]",
        "ipv4" => "[REDACTED_IP]",
        "aws_access_key_id" => "[REDACTED_AWS_KEY]",
        _ => "[REDACTED]",
    }
}

/// Why [`redact`] refused to run.
#[derive(Debug, thiserror::Error)]
pub enum RedactError {
    /// Input exceeded [`MAX_INPUT_BYTES`].
    #[error("input text is {actual} bytes, which exceeds the {limit}-byte limit")]
    TooLarge { limit: usize, actual: usize },
    /// The `types` argument named categories that are not in [`KNOWN_TYPES`].
    #[error("unknown PII type(s): {}. valid types are: {}", .unknown.join(", "), KNOWN_TYPES.join(", "))]
    UnknownTypes { unknown: Vec<String> },
}

/// The result of a successful redaction pass.
#[derive(Debug)]
pub struct RedactionResult {
    /// The input text with every accepted match replaced by its placeholder.
    pub redacted: String,
    /// Count of redactions per enabled category (categories with zero hits are
    /// included, so every enabled type appears).
    pub counts: BTreeMap<&'static str, u64>,
    /// Total number of redactions across all categories.
    pub total: u64,
}

/// One compiled detector.
struct Pattern {
    category: &'static str,
    /// Lower wins when two candidates start at the same offset.
    precedence: u8,
    re: Regex,
    /// Capture group whose span is the value to redact (0 = whole match).
    group: usize,
    /// Validate the matched digits with the Luhn checksum before accepting.
    luhn: bool,
}

/// The compiled pattern set, built once on first use.
fn patterns() -> &'static [Pattern] {
    static PATTERNS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // Email — highest precedence so an address's own characters are not
            // re-scanned as other categories.
            Pattern {
                category: "email",
                precedence: 0,
                re: Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").unwrap(),
                group: 0,
                luhn: false,
            },
            // AWS access key id: AKIA / ASIA + 16 uppercase alphanumerics.
            Pattern {
                category: "aws_access_key_id",
                precedence: 1,
                re: Regex::new(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b").unwrap(),
                group: 0,
                luhn: false,
            },
            // Credit card: a 13–19 digit run, optionally single space/dash
            // separated. Luhn-validated before it is accepted.
            Pattern {
                category: "credit_card",
                precedence: 2,
                re: Regex::new(r"\b\d(?:[ \-]?\d){12,18}\b").unwrap(),
                group: 0,
                luhn: true,
            },
            // US SSN, separated form `123-45-6789`, `123.45.6789`, or
            // `123 45 6789`. The 3-2-4 grouping is distinctive (phone is 3-3-4),
            // so it is always accepted. A dash, dot, or single space separator is
            // allowed between groups.
            Pattern {
                category: "us_ssn",
                precedence: 3,
                re: Regex::new(r"\b\d{3}[-. ]\d{2}[-. ]\d{4}\b").unwrap(),
                group: 0,
                luhn: false,
            },
            // US SSN, bare 9 digits — only when an SSN context word is adjacent.
            // Group 1 is the digits; the context word is not redacted.
            Pattern {
                category: "us_ssn",
                precedence: 3,
                re: Regex::new(r"(?i)\b(?:ssn|social security(?:\s+number)?)\b\D{0,10}(\d{9})\b")
                    .unwrap(),
                group: 1,
                luhn: false,
            },
            // IPv4 dotted quad, each octet 0–255.
            Pattern {
                category: "ipv4",
                precedence: 4,
                re: Regex::new(
                    r"\b(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\b",
                )
                .unwrap(),
                group: 0,
                luhn: false,
            },
            // NANP phone numbers: (555) 123-4567, 555-123-4567, 555.123.4567,
            // +1 555 123 4567, +1-555-123-4567. A separator is required between
            // groups so a bare 10-digit run is not swept up.
            Pattern {
                category: "phone",
                precedence: 5,
                re: Regex::new(
                    r"(?:\+?1[ .\-]?)?(?:\(\d{3}\)[ ]?|\d{3}[ .\-])\d{3}[ .\-]\d{4}\b",
                )
                .unwrap(),
                group: 0,
                luhn: false,
            },
        ]
    })
}

/// A resolved candidate span in the input.
struct Span {
    start: usize,
    end: usize,
    category: &'static str,
    precedence: u8,
}

/// The Luhn checksum: sum digits, doubling every second from the right, and
/// require the total to be a multiple of 10. Non-digit characters are ignored.
fn luhn_valid(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if digits.len() < 13 || digits.len() > 19 {
        return false;
    }
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        if i % 2 == 1 {
            let doubled = d * 2;
            sum += if doubled > 9 { doubled - 9 } else { doubled };
        } else {
            sum += d;
        }
    }
    sum % 10 == 0
}

/// Redact PII from `text`.
///
/// `types` optionally restricts the scan to a subset of [`KNOWN_TYPES`]; `None`
/// scans for all of them. Unknown type names produce
/// [`RedactError::UnknownTypes`]; oversized input produces
/// [`RedactError::TooLarge`].
pub fn redact(text: &str, types: Option<&[String]>) -> Result<RedactionResult, RedactError> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(RedactError::TooLarge {
            limit: MAX_INPUT_BYTES,
            actual: text.len(),
        });
    }

    // Resolve the enabled category set, validating any caller-supplied names.
    let enabled: Vec<&'static str> = match types {
        None => KNOWN_TYPES.to_vec(),
        Some(requested) => {
            let mut unknown = Vec::new();
            let mut enabled = Vec::new();
            for name in requested {
                match KNOWN_TYPES.iter().find(|k| **k == name.as_str()) {
                    Some(canonical) => {
                        if !enabled.contains(canonical) {
                            enabled.push(*canonical);
                        }
                    }
                    None => unknown.push(name.clone()),
                }
            }
            if !unknown.is_empty() {
                return Err(RedactError::UnknownTypes { unknown });
            }
            enabled
        }
    };

    // Seed the counts map so every enabled category is reported, even at zero.
    let mut counts: BTreeMap<&'static str, u64> =
        enabled.iter().map(|c| (*c, 0u64)).collect();

    // Gather every candidate span from every enabled pattern.
    let mut spans: Vec<Span> = Vec::new();
    for pat in patterns() {
        if !enabled.contains(&pat.category) {
            continue;
        }
        for caps in pat.re.captures_iter(text) {
            let Some(m) = caps.get(pat.group) else {
                continue;
            };
            if pat.luhn && !luhn_valid(&text[m.start()..m.end()]) {
                continue;
            }
            spans.push(Span {
                start: m.start(),
                end: m.end(),
                category: pat.category,
                precedence: pat.precedence,
            });
        }
    }

    // Order: earliest start first; at a tie, higher precedence (lower number),
    // then the longer span.
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.precedence.cmp(&b.precedence))
            .then((b.end - b.start).cmp(&(a.end - a.start)))
    });

    // Single left-to-right pass: accept a span only if it starts at or after
    // the cursor, so overlapping candidates never double-count.
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut total = 0u64;
    for span in &spans {
        if span.start < cursor {
            continue;
        }
        out.push_str(&text[cursor..span.start]);
        out.push_str(placeholder(span.category));
        if let Some(n) = counts.get_mut(span.category) {
            *n += 1;
        }
        total += 1;
        cursor = span.end;
    }
    out.push_str(&text[cursor..]);

    Ok(RedactionResult {
        redacted: out,
        counts,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(text: &str) -> RedactionResult {
        redact(text, None).unwrap()
    }

    #[test]
    fn redacts_the_sample() {
        let r = all("Email me at jane.doe@example.com or call (555) 123-4567. SSN 123-45-6789, card 4111 1111 1111 1111, from 10.0.0.5, key AKIAIOSFODNN7EXAMPLE.");
        assert!(r.redacted.contains("[REDACTED_EMAIL]"));
        assert!(r.redacted.contains("[REDACTED_PHONE]"));
        assert!(r.redacted.contains("[REDACTED_SSN]"));
        assert!(r.redacted.contains("[REDACTED_CC]"));
        assert!(r.redacted.contains("[REDACTED_IP]"));
        assert!(r.redacted.contains("[REDACTED_AWS_KEY]"));
        assert_eq!(r.counts["email"], 1);
        assert_eq!(r.counts["phone"], 1);
        assert_eq!(r.counts["us_ssn"], 1);
        assert_eq!(r.counts["credit_card"], 1);
        assert_eq!(r.counts["ipv4"], 1);
        assert_eq!(r.counts["aws_access_key_id"], 1);
        assert_eq!(r.total, 6);
        assert!(!r.redacted.contains("jane.doe@example.com"));
        assert!(!r.redacted.contains("4111"));
    }

    #[test]
    fn luhn_invalid_card_is_not_redacted() {
        // 16 digits, fails Luhn.
        let r = all("card 1234 5678 9012 3456 here");
        assert_eq!(r.counts["credit_card"], 0);
        assert!(r.redacted.contains("1234 5678 9012 3456"));
        assert!(!r.redacted.contains("[REDACTED_CC]"));
    }

    #[test]
    fn luhn_valid_visa_is_redacted() {
        let r = all("4111111111111111");
        assert_eq!(r.counts["credit_card"], 1);
        assert_eq!(r.redacted, "[REDACTED_CC]");
    }

    #[test]
    fn phone_formats() {
        for p in ["(555) 123-4567", "555-123-4567", "+1 555 123 4567", "555.123.4567"] {
            let r = all(p);
            assert_eq!(r.counts["phone"], 1, "failed on {p}");
        }
    }

    #[test]
    fn ssn_dashed_and_context_bare() {
        assert_eq!(all("123-45-6789").counts["us_ssn"], 1);
        assert_eq!(all("SSN 123456789").counts["us_ssn"], 1);
        // Bare 9 digits without context are left alone.
        assert_eq!(all("order 123456789 shipped").counts["us_ssn"], 0);
    }

    #[test]
    fn ssn_dot_and_space_separators() {
        // The 3-2-4 grouping is now caught with dot or single-space separators,
        // not only dashes.
        let space = all("123 45 6789");
        assert_eq!(space.counts["us_ssn"], 1);
        assert_eq!(space.redacted, "[REDACTED_SSN]");

        let dot = all("123.45.6789");
        assert_eq!(dot.counts["us_ssn"], 1);
        assert_eq!(dot.redacted, "[REDACTED_SSN]");
    }

    #[test]
    fn phone_is_not_mishit_as_ssn() {
        // A 3-3-4 phone number stays a phone, never an SSN (phone is 3-3-4,
        // SSN is 3-2-4).
        let r = all("(555) 123-4567");
        assert_eq!(r.counts["phone"], 1);
        assert_eq!(r.counts["us_ssn"], 0);
        assert_eq!(r.redacted, "[REDACTED_PHONE]");
    }

    #[test]
    fn ordinary_runs_are_not_mishit_as_ssn() {
        // An ISO date (4-2-2) and a plain dotted version string do not match the
        // 3-2-4 SSN shape.
        assert_eq!(all("shipped on 2026-08-14").counts["us_ssn"], 0);
        assert_eq!(all("upgraded to 1.2.3 today").counts["us_ssn"], 0);
    }

    #[test]
    fn ipv4_octet_bounds() {
        assert_eq!(all("10.0.0.5").counts["ipv4"], 1);
        // 999 is not a valid octet.
        assert_eq!(all("999.1.1.1").counts["ipv4"], 0);
    }

    #[test]
    fn types_filter_restricts_scan() {
        let types = vec!["email".to_string()];
        let r = redact("a@b.com and 10.0.0.5", Some(&types)).unwrap();
        assert_eq!(r.counts["email"], 1);
        assert!(!r.counts.contains_key("ipv4"));
        assert!(r.redacted.contains("10.0.0.5"));
    }

    #[test]
    fn unknown_type_errors() {
        let types = vec!["email".to_string(), "passport".to_string()];
        let err = redact("x", Some(&types)).unwrap_err();
        assert!(matches!(err, RedactError::UnknownTypes { .. }));
    }

    #[test]
    fn oversized_input_errors() {
        let big = "a".repeat(MAX_INPUT_BYTES + 1);
        assert!(matches!(
            redact(&big, None).unwrap_err(),
            RedactError::TooLarge { .. }
        ));
    }
}
