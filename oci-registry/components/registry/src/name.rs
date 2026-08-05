//! Repository name and tag validation per the OCI distribution spec.
//!
//! Names map directly onto on-disk directory paths, so validation is also
//! the path-traversal guard: only lowercase alnum and `._-`, components
//! separated by `/`, no `..`, no empty components.

/// Validate a repository name (the `<name>` in `/v2/<name>/...`).
///
/// Spec grammar:
///   name      := component ('/' component)*
///   component := alphanum (separator alphanum)*
///   separator := '.' | '_' | '__' | '-'+
///   alphanum  := [a-z0-9]+
///
/// Total length is capped at 255 bytes.
pub fn is_valid_repository(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    name.split('/').all(is_valid_component)
}

fn is_valid_component(c: &str) -> bool {
    if c.is_empty() {
        return false;
    }
    let bytes = c.as_bytes();
    // Must start and end with an alphanumeric.
    if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    let mut prev_sep = false;
    for &b in bytes {
        if is_alnum(b) {
            prev_sep = false;
        } else if b == b'.' || b == b'_' || b == b'-' {
            // No two adjacent '.' (the grammar allows '__' and '-+' runs but
            // not '..'); we conservatively allow runs of '_' and '-' and a
            // single '.' between alphanumerics, which covers real-world names.
            if prev_sep && b == b'.' {
                return false;
            }
            prev_sep = true;
        } else {
            return false;
        }
    }
    true
}

fn is_alnum(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit()
}

/// Validate a tag reference: `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
pub fn is_valid_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphanumeric() || first == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        for n in [
            "nginx",
            "library/nginx",
            "cosmonic-labs/oci-registry",
            "a/b/c/d",
            "foo.bar/baz_qux",
            "x1.2.3",
        ] {
            assert!(is_valid_repository(n), "{n} should be valid");
        }
    }

    #[test]
    fn invalid_names() {
        for n in [
            "",
            "Nginx", // uppercase
            "/leading",
            "trailing/",
            "double//slash",
            "../escape",
            "foo/..",
            ".dotstart",
            "dotend.",
            "a..b",
            &"x".repeat(256),
        ] {
            assert!(!is_valid_repository(n), "{n} should be invalid");
        }
    }

    #[test]
    fn tags() {
        assert!(is_valid_tag("latest"));
        assert!(is_valid_tag("v1.1.0"));
        assert!(is_valid_tag("_underscore"));
        assert!(!is_valid_tag(""));
        assert!(!is_valid_tag(".dotstart"));
        assert!(!is_valid_tag("has space"));
        assert!(!is_valid_tag(&"x".repeat(129)));
    }
}
