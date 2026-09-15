//! Content digests. The registry is content-addressed: every blob and
//! manifest is identified by `sha256:<64 hex>`. We compute sha256 and
//! validate the canonical `algorithm:hex` form for both sha256 and sha512
//! (the two algorithms the OCI image-spec registers), but only ever
//! *produce* sha256.

use sha2::{Digest, Sha256};

/// Compute the canonical `sha256:<hex>` digest of `bytes`.
pub fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{:x}", h.finalize())
}

/// Split a digest string into `(algorithm, hex)` if it is well-formed.
///
/// Accepts the OCI-registered algorithms `sha256` (64 hex) and `sha512`
/// (128 hex). The hex must be lowercase, matching the canonical form the
/// distribution spec requires for path components.
pub fn parse(digest: &str) -> Option<(&str, &str)> {
    let (algo, hex) = digest.split_once(':')?;
    let want_len = match algo {
        "sha256" => 64,
        "sha512" => 128,
        _ => return None,
    };
    if hex.len() == want_len
        && hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Some((algo, hex))
    } else {
        None
    }
}

/// True if `digest` is a syntactically valid canonical digest.
pub fn is_valid(digest: &str) -> bool {
    parse(digest).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // echo -n "" | sha256sum
        assert_eq!(
            sha256(b""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"hello"),
            "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn parse_accepts_valid_and_rejects_invalid() {
        assert!(is_valid(
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        assert!(is_valid(&format!("sha512:{}", "a".repeat(128))));
        // wrong length
        assert!(!is_valid("sha256:abcd"));
        // uppercase not canonical
        assert!(!is_valid(
            "sha256:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855"
        ));
        // unknown algo
        assert!(!is_valid(&("md5:".to_string() + &"a".repeat(32))));
        // no colon
        assert!(!is_valid("deadbeef"));
    }
}
