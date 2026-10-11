//! Routing and handlers for the OCI Distribution Spec v1.1 API plus a small
//! browse UI/JSON surface for local development.
//!
//! `dispatch` is pure: given a [`Storage`] and a [`Request`] it returns a
//! [`Response`]. No wasm, no globals — the integration tests drive it
//! directly against a tempdir.

use crate::digest;
use crate::error::ErrorCode;
use crate::http::{Method, Request, Response};
use crate::name;
use crate::storage::{FinalizeError, Storage};
use crate::ui;

pub const API_VERSION_HEADER: (&str, &str) = ("docker-distribution-api-version", "registry/2.0");

/// Default page size for catalog/tags listings when `n` is absent.
const DEFAULT_PAGE: usize = 1000;

/// Top-level router.
pub fn dispatch(store: &Storage, req: &Request) -> Response {
    // Browse UI / health / JSON browse — local-dev conveniences that live
    // outside the /v2 tree.
    match req.path.as_str() {
        "/" | "/ui" | "/index.html" => return ui::index(store, req),
        "/healthz" => return health(store),
        "/api/repositories" => return ui::api_repositories(store),
        _ => {}
    }
    if let Some(rest) = req.path.strip_prefix("/api/repository/") {
        return ui::api_repository(store, rest);
    }

    // Everything else must be under /v2.
    let Some(v2) = req.path.strip_prefix("/v2") else {
        return Response::error(ErrorCode::Unsupported, "not found");
    };
    let v2 = v2.strip_prefix('/').unwrap_or(v2);

    match parse_route(v2) {
        Route::Base => base(req),
        Route::Catalog => catalog(store, req),
        Route::TagsList { name } => tags_list(store, req, name),
        Route::Manifest { name, reference } => manifest(store, req, name, reference),
        Route::Referrers { name, digest } => referrers(store, req, name, digest),
        Route::Upload { name, id } => upload(store, req, name, id),
        Route::Blob { name, digest } => blob(store, req, name, digest),
        Route::NotFound => Response::error(ErrorCode::Unsupported, "unsupported path"),
    }
}

enum Route<'a> {
    Base,
    Catalog,
    TagsList { name: &'a str },
    Manifest { name: &'a str, reference: &'a str },
    Referrers { name: &'a str, digest: &'a str },
    Upload { name: &'a str, id: &'a str },
    Blob { name: &'a str, digest: &'a str },
    NotFound,
}

/// Parse the path beneath `/v2/`. The reserved markers (`manifests`, `blobs`,
/// `tags/list`, `referrers`) are positional, so we split on the rightmost
/// marker; everything before it is the (possibly slash-bearing) repo name.
fn parse_route(p: &str) -> Route<'_> {
    if p.is_empty() {
        return Route::Base;
    }
    if p == "_catalog" {
        return Route::Catalog;
    }
    if let Some(name) = p.strip_suffix("/tags/list") {
        return Route::TagsList { name };
    }
    if let Some(i) = p.rfind("/manifests/") {
        return Route::Manifest {
            name: &p[..i],
            reference: &p[i + "/manifests/".len()..],
        };
    }
    if let Some(i) = p.rfind("/referrers/") {
        return Route::Referrers {
            name: &p[..i],
            digest: &p[i + "/referrers/".len()..],
        };
    }
    if let Some(i) = p.rfind("/blobs/uploads") {
        let name = &p[..i];
        let rest = &p[i + "/blobs/uploads".len()..];
        let id = rest.strip_prefix('/').unwrap_or("");
        return Route::Upload { name, id };
    }
    if let Some(i) = p.rfind("/blobs/") {
        return Route::Blob {
            name: &p[..i],
            digest: &p[i + "/blobs/".len()..],
        };
    }
    Route::NotFound
}

// ----- handlers -------------------------------------------------------------

fn base(req: &Request) -> Response {
    match req.method {
        Method::Get | Method::Head => Response::new(200)
            .with_header(API_VERSION_HEADER.0, API_VERSION_HEADER.1)
            .with_body("application/json", b"{}".to_vec()),
        _ => Response::error(ErrorCode::Unsupported, "method not allowed"),
    }
}

fn health(store: &Storage) -> Response {
    Response::json(
        200,
        serde_json::json!({
            "status": "ok",
            "root": store.root().display().to_string(),
            "repositories": store.list_repositories().len(),
        }),
    )
}

fn catalog(store: &Storage, req: &Request) -> Response {
    if req.method != Method::Get {
        return Response::error(ErrorCode::Unsupported, "method not allowed");
    }
    let all = store.list_repositories();
    let (page, link) = paginate(&all, req, "/v2/_catalog");
    let mut resp = Response::json(200, serde_json::json!({ "repositories": page }));
    if let Some(link) = link {
        resp = resp.with_header("link", link);
    }
    cors(resp)
}

fn tags_list(store: &Storage, req: &Request, name: &str) -> Response {
    if req.method != Method::Get {
        return Response::error(ErrorCode::Unsupported, "method not allowed");
    }
    if !name::is_valid_repository(name) {
        return Response::error(ErrorCode::NameInvalid, "invalid repository name");
    }
    if !store.repo_exists(name) {
        return Response::error(
            ErrorCode::NameUnknown,
            "repository name not known to registry",
        );
    }
    let all = store.list_tags(name);
    let (page, link) = paginate(&all, req, &format!("/v2/{name}/tags/list"));
    let mut resp = Response::json(200, serde_json::json!({ "name": name, "tags": page }));
    if let Some(link) = link {
        resp = resp.with_header("link", link);
    }
    cors(resp)
}

fn manifest(store: &Storage, req: &Request, name: &str, reference: &str) -> Response {
    if !name::is_valid_repository(name) {
        return Response::error(ErrorCode::NameInvalid, "invalid repository name");
    }
    match req.method {
        Method::Get | Method::Head => match store.get_manifest(name, reference) {
            Some(m) => {
                let mut resp = Response::new(200)
                    .with_header("content-type", m.media_type)
                    .with_header("docker-content-digest", m.digest)
                    .with_header("content-length", m.bytes.len().to_string());
                if req.method == Method::Get {
                    resp.body = m.bytes;
                }
                resp
            }
            None => Response::error(ErrorCode::ManifestUnknown, "manifest unknown"),
        },
        Method::Put => put_manifest(store, req, name, reference),
        Method::Delete => {
            let existed = if digest::is_valid(reference) {
                store.delete_manifest(name, reference).unwrap_or(false)
            } else {
                store.delete_tag(name, reference).unwrap_or(false)
            };
            if existed {
                Response::new(202)
            } else {
                Response::error(ErrorCode::ManifestUnknown, "manifest unknown")
            }
        }
        _ => Response::error(ErrorCode::Unsupported, "method not allowed"),
    }
}

fn put_manifest(store: &Storage, req: &Request, name: &str, reference: &str) -> Response {
    // Must be valid JSON.
    if serde_json::from_slice::<serde_json::Value>(&req.body).is_err() {
        return Response::error(ErrorCode::ManifestInvalid, "manifest is not valid JSON");
    }
    let media_type = req
        .content_type
        .clone()
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_string());

    let reference_is_digest = digest::is_valid(reference);
    if !reference_is_digest && !name::is_valid_tag(reference) {
        return Response::error(ErrorCode::ManifestInvalid, "invalid tag");
    }
    let computed = digest::sha256(&req.body);
    if reference_is_digest && reference != computed {
        return Response::error(
            ErrorCode::DigestInvalid,
            "provided digest did not match the manifest content",
        );
    }
    let tag = if reference_is_digest {
        None
    } else {
        Some(reference)
    };
    match store.put_manifest(name, &req.body, &media_type, tag) {
        Ok(dgst) => Response::new(201)
            .with_header("location", format!("/v2/{name}/manifests/{dgst}"))
            .with_header("docker-content-digest", dgst),
        Err(e) => Response::error(ErrorCode::ManifestInvalid, &format!("write failed: {e}")),
    }
}

fn blob(store: &Storage, req: &Request, name: &str, dgst: &str) -> Response {
    if !name::is_valid_repository(name) {
        return Response::error(ErrorCode::NameInvalid, "invalid repository name");
    }
    if !digest::is_valid(dgst) {
        return Response::error(ErrorCode::DigestInvalid, "invalid digest");
    }
    match req.method {
        Method::Get => match store.read_blob(dgst) {
            Some(bytes) => Response::new(200)
                .with_header("docker-content-digest", dgst)
                .with_header("content-length", bytes.len().to_string())
                .with_body("application/octet-stream", bytes),
            None => Response::error(ErrorCode::BlobUnknown, "blob unknown to registry"),
        },
        Method::Head => match store.blob_size(dgst) {
            Some(size) => Response::new(200)
                .with_header("docker-content-digest", dgst)
                .with_header("content-length", size.to_string())
                .with_header("content-type", "application/octet-stream"),
            None => Response::error(ErrorCode::BlobUnknown, "blob unknown to registry"),
        },
        Method::Delete => {
            if store.delete_blob(dgst).unwrap_or(false) {
                Response::new(202).with_header("docker-content-digest", dgst)
            } else {
                Response::error(ErrorCode::BlobUnknown, "blob unknown to registry")
            }
        }
        _ => Response::error(ErrorCode::Unsupported, "method not allowed"),
    }
}

fn upload(store: &Storage, req: &Request, name: &str, id: &str) -> Response {
    if !name::is_valid_repository(name) {
        return Response::error(ErrorCode::NameInvalid, "invalid repository name");
    }
    match req.method {
        // Start a session — or do a monolithic/mount shortcut.
        Method::Post if id.is_empty() => start_upload(store, req, name),
        Method::Patch => patch_upload(store, req, name, id),
        Method::Put => put_upload(store, req, name, id),
        Method::Get => match store.upload_size(id) {
            Some(size) => upload_accepted(204, name, id, size),
            None => Response::error(ErrorCode::BlobUploadUnknown, "upload unknown"),
        },
        Method::Delete => {
            if store.cancel_upload(id).unwrap_or(false) {
                Response::new(204)
            } else {
                Response::error(ErrorCode::BlobUploadUnknown, "upload unknown")
            }
        }
        _ => Response::error(ErrorCode::Unsupported, "method not allowed"),
    }
}

fn start_upload(store: &Storage, req: &Request, name: &str) -> Response {
    // Cross-repo mount: blobs are global, so a mount succeeds iff we already
    // hold the blob. Otherwise fall through to a normal session per spec.
    if let Some(mount) = req.query_get("mount") {
        if digest::is_valid(mount) && store.blob_exists(mount) {
            return Response::new(201)
                .with_header("location", format!("/v2/{name}/blobs/{mount}"))
                .with_header("docker-content-digest", mount);
        }
    }
    // Monolithic single-POST upload (POST .../uploads/?digest=sha256:...).
    if let Some(want) = req.query_get("digest") {
        if !digest::is_valid(want) {
            return Response::error(ErrorCode::DigestInvalid, "invalid digest");
        }
        let computed = digest::sha256(&req.body);
        if computed != want {
            return Response::error(ErrorCode::DigestInvalid, "digest did not match content");
        }
        return match store.write_blob(&req.body) {
            Ok(dgst) => Response::new(201)
                .with_header("location", format!("/v2/{name}/blobs/{dgst}"))
                .with_header("docker-content-digest", dgst),
            Err(e) => Response::error(ErrorCode::BlobUploadInvalid, &format!("write failed: {e}")),
        };
    }
    match store.create_upload() {
        Ok(id) => upload_accepted(202, name, &id, 0),
        Err(e) => Response::error(ErrorCode::BlobUploadInvalid, &format!("create failed: {e}")),
    }
}

fn patch_upload(store: &Storage, req: &Request, name: &str, id: &str) -> Response {
    if !store.upload_exists(id) {
        return Response::error(ErrorCode::BlobUploadUnknown, "upload unknown");
    }
    // If a Content-Range was supplied, its start must equal the current size.
    if let Some(cr) = &req.content_range {
        if let Some(start) = parse_range_start(cr) {
            let current = store.upload_size(id).unwrap_or(0);
            if start != current {
                return Response::new(416)
                    .with_header("range", format!("0-{}", current.saturating_sub(1)))
                    .with_header("docker-upload-uuid", id.to_string());
            }
        }
    }
    match store.append_upload(id, &req.body) {
        Ok(size) => upload_accepted(202, name, id, size),
        Err(e) => Response::error(ErrorCode::BlobUploadInvalid, &format!("append failed: {e}")),
    }
}

fn put_upload(store: &Storage, req: &Request, name: &str, id: &str) -> Response {
    let Some(want) = req.query_get("digest") else {
        return Response::error(ErrorCode::DigestInvalid, "digest query parameter required");
    };
    match store.finalize_upload(id, &req.body, want) {
        Ok(dgst) => Response::new(201)
            .with_header("location", format!("/v2/{name}/blobs/{dgst}"))
            .with_header("docker-content-digest", dgst),
        Err(FinalizeError::UnknownUpload) => {
            Response::error(ErrorCode::BlobUploadUnknown, "upload unknown")
        }
        Err(FinalizeError::InvalidDigest) => {
            Response::error(ErrorCode::DigestInvalid, "invalid digest")
        }
        Err(FinalizeError::DigestMismatch { got, .. }) => Response::error(
            ErrorCode::DigestInvalid,
            &format!("content digest {got} did not match requested {want}"),
        ),
        Err(FinalizeError::Io(e)) => Response::error(
            ErrorCode::BlobUploadInvalid,
            &format!("finalize failed: {e}"),
        ),
    }
}

/// 202/204 upload response with the standard Location/Range/UUID headers.
fn upload_accepted(status: u16, name: &str, id: &str, size: u64) -> Response {
    let range_end = size.saturating_sub(1);
    Response::new(status)
        .with_header("location", format!("/v2/{name}/blobs/uploads/{id}"))
        .with_header("range", format!("0-{range_end}"))
        .with_header("docker-upload-uuid", id.to_string())
        .with_header("content-length", "0")
}

fn referrers(store: &Storage, req: &Request, name: &str, subject: &str) -> Response {
    if req.method != Method::Get {
        return Response::error(ErrorCode::Unsupported, "method not allowed");
    }
    if !name::is_valid_repository(name) {
        return Response::error(ErrorCode::NameInvalid, "invalid repository name");
    }
    if !digest::is_valid(subject) {
        return Response::error(ErrorCode::DigestInvalid, "invalid digest");
    }
    let filter = req.query_get("artifactType");
    let mut manifests = Vec::new();
    let mut applied_filter = false;
    for mdgst in store.list_manifest_digests(name) {
        let Some(m) = store.get_manifest(name, &mdgst) else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&m.bytes) else {
            continue;
        };
        let refers = json
            .get("subject")
            .and_then(|s| s.get("digest"))
            .and_then(|d| d.as_str())
            == Some(subject);
        if !refers {
            continue;
        }
        let artifact_type = json
            .get("artifactType")
            .and_then(|a| a.as_str())
            .map(String::from)
            .or_else(|| {
                json.get("config")
                    .and_then(|c| c.get("mediaType"))
                    .and_then(|c| c.as_str())
                    .map(String::from)
            });
        if let Some(want) = filter {
            applied_filter = true;
            if artifact_type.as_deref() != Some(want) {
                continue;
            }
        }
        let mut desc = serde_json::json!({
            "mediaType": m.media_type,
            "digest": m.digest,
            "size": m.bytes.len(),
        });
        if let Some(at) = artifact_type {
            desc["artifactType"] = serde_json::Value::String(at);
        }
        if let Some(ann) = json.get("annotations") {
            desc["annotations"] = ann.clone();
        }
        manifests.push(desc);
    }
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    });
    let mut resp = Response::new(200).with_body(
        "application/vnd.oci.image.index.v1+json",
        serde_json::to_vec(&index).unwrap_or_default(),
    );
    if applied_filter {
        resp = resp.with_header("oci-filters-applied", "artifactType");
    }
    resp
}

// ----- helpers --------------------------------------------------------------

/// Apply `n`/`last` pagination to a sorted list. Returns the page plus an
/// optional RFC5988 `Link` header value for the next page.
fn paginate(all: &[String], req: &Request, base_path: &str) -> (Vec<String>, Option<String>) {
    let n = req
        .query_get("n")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_PAGE);
    let last = req.query_get("last");
    let start = match last {
        Some(l) => all
            .iter()
            .position(|x| x.as_str() == l)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    };
    let end = (start + n).min(all.len());
    let page: Vec<String> = all[start..end].to_vec();
    let link = if end < all.len() {
        page.last()
            .map(|tail| format!("<{base_path}?n={n}&last={tail}>; rel=\"next\"",))
    } else {
        None
    };
    (page, link)
}

/// Parse the start offset from a `Content-Range: <start>-<end>` header.
fn parse_range_start(cr: &str) -> Option<u64> {
    let cr = cr.trim();
    let core = cr.strip_prefix("bytes ").unwrap_or(cr);
    let core = core.split('/').next().unwrap_or(core);
    core.split('-').next()?.trim().parse().ok()
}

/// Add permissive CORS so the browse UI can fetch JSON from a file:// or other
/// origin during local development.
fn cors(resp: Response) -> Response {
    resp.with_header("access-control-allow-origin", "*")
}
