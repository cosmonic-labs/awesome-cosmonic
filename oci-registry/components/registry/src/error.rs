//! OCI distribution-spec error codes and the JSON error envelope.
//!
//! Every failure the API surfaces maps to one of the registered codes with
//! its conventional HTTP status. The body is always
//! `{"errors":[{"code","message"}]}`.

/// A distribution-spec error code, with its HTTP status and wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BlobUnknown,
    BlobUploadInvalid,
    BlobUploadUnknown,
    DigestInvalid,
    ManifestBlobUnknown,
    ManifestInvalid,
    ManifestUnknown,
    NameInvalid,
    NameUnknown,
    SizeInvalid,
    Unauthorized,
    Denied,
    Unsupported,
    TooManyRequests,
}

impl ErrorCode {
    pub fn status(self) -> u16 {
        use ErrorCode::*;
        match self {
            BlobUnknown | ManifestBlobUnknown | ManifestUnknown | NameUnknown
            | BlobUploadUnknown => 404,
            DigestInvalid | ManifestInvalid | NameInvalid | SizeInvalid | BlobUploadInvalid => 400,
            Unauthorized => 401,
            Denied => 403,
            Unsupported => 405,
            TooManyRequests => 429,
        }
    }

    pub fn code(self) -> &'static str {
        use ErrorCode::*;
        match self {
            BlobUnknown => "BLOB_UNKNOWN",
            BlobUploadInvalid => "BLOB_UPLOAD_INVALID",
            BlobUploadUnknown => "BLOB_UPLOAD_UNKNOWN",
            DigestInvalid => "DIGEST_INVALID",
            ManifestBlobUnknown => "MANIFEST_BLOB_UNKNOWN",
            ManifestInvalid => "MANIFEST_INVALID",
            ManifestUnknown => "MANIFEST_UNKNOWN",
            NameInvalid => "NAME_INVALID",
            NameUnknown => "NAME_UNKNOWN",
            SizeInvalid => "SIZE_INVALID",
            Unauthorized => "UNAUTHORIZED",
            Denied => "DENIED",
            Unsupported => "UNSUPPORTED",
            TooManyRequests => "TOOMANYREQUESTS",
        }
    }

    /// Render the JSON error body for this code with a human message.
    pub fn body(self, message: &str) -> Vec<u8> {
        let v = serde_json::json!({
            "errors": [{
                "code": self.code(),
                "message": message,
            }]
        });
        serde_json::to_vec(&v).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_shape() {
        let b = ErrorCode::ManifestUnknown.body("no such manifest");
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["errors"][0]["code"], "MANIFEST_UNKNOWN");
        assert_eq!(v["errors"][0]["message"], "no such manifest");
        assert_eq!(ErrorCode::ManifestUnknown.status(), 404);
    }
}
