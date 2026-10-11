//! An OCI Distribution Spec v1.1 registry packaged as a single wasmCloud v2
//! component.
//!
//! The component exports `wasi:http/incoming-handler` and is a pure reactor:
//! the host instantiates it per request and tears it down afterwards, so it
//! scales to zero when idle. All durable state lives on the disk volume
//! mounted at `/data` (override with the `REGISTRY_ROOT` env config), which is
//! what makes the stateless-per-request model possible.
//!
//! The modules below are split so the entire request path is testable on the
//! host: [`digest`], [`name`], [`error`], [`http`], [`storage`], [`oci`], and
//! [`ui`] are plain `std` Rust. Only the thin glue in this file is wasm-only.

pub mod digest;
pub mod error;
pub mod http;
pub mod name;
pub mod oci;
pub mod storage;
pub mod ui;

#[cfg(test)]
mod tests;

/// Default storage root — the volume mount path in the Workload manifest.
pub const DEFAULT_ROOT: &str = "/data";

// ---------------------------------------------------------------------------
// wasm32 entry point: translate wasi:http <-> our transport-agnostic types.
// ---------------------------------------------------------------------------
#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::io::Read;

    use wasmcloud_component::http as wc;

    use crate::http::{Request, Response};
    use crate::storage::Storage;

    struct Component;

    wc::export!(Component);

    impl wc::Server for Component {
        fn handle(request: wc::IncomingRequest) -> wc::Result<wc::Response<impl wc::OutgoingBody>> {
            let resp = handle_inner(request);
            let mut builder = wc::Response::builder().status(resp.status);
            for (name, value) in &resp.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            Ok(builder.body(resp.body).unwrap_or_else(|_| {
                wc::Response::builder()
                    .status(500)
                    .body(b"header encoding error".to_vec())
                    .expect("static 500 response always builds")
            }))
        }
    }

    fn handle_inner(request: wc::IncomingRequest) -> Response {
        let (parts, mut body) = request.into_parts();

        let mut buf = Vec::new();
        if body.read_to_end(&mut buf).is_err() {
            return Response::error(
                crate::error::ErrorCode::BlobUploadInvalid,
                "failed to read request body",
            );
        }

        let mut req = Request::new(
            parts.method.as_str(),
            parts.uri.path(),
            parts.uri.query(),
            buf,
        );
        req.content_type = header(&parts.headers, "content-type");
        req.content_range = header(&parts.headers, "content-range");
        req.host = header(&parts.headers, "host");

        let root =
            std::env::var("REGISTRY_ROOT").unwrap_or_else(|_| crate::DEFAULT_ROOT.to_string());
        let store = match Storage::open(&root) {
            Ok(s) => s,
            Err(e) => {
                return Response::error(
                    crate::error::ErrorCode::BlobUploadInvalid,
                    &format!("storage unavailable at {root}: {e}"),
                )
            }
        };

        crate::oci::dispatch(&store, &req)
    }

    fn header(headers: &wc::HeaderMap, name: &str) -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }
}
