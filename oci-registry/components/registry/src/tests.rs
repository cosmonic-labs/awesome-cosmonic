//! End-to-end tests of the registry dispatch against a real tempdir. These
//! exercise the same code path the wasm component runs, minus the wasi:http
//! translation, so they cover the OCI spec behaviour directly.

use crate::digest;
use crate::http::{Request, Response};
use crate::oci::dispatch;
use crate::storage::Storage;

fn store() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().unwrap();
    let s = Storage::open(dir.path()).unwrap();
    (dir, s)
}

fn req(method: &str, path: &str) -> Request {
    let (p, q) = match path.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path, None),
    };
    Request::new(method, p, q, Vec::new())
}

fn req_body(method: &str, path: &str, ct: &str, body: Vec<u8>) -> Request {
    let mut r = req(method, path);
    r.body = body;
    r.content_type = Some(ct.to_string());
    r
}

fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
    resp.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[test]
fn base_endpoint_reports_v2() {
    let (_d, s) = store();
    let r = dispatch(&s, &req("GET", "/v2/"));
    assert_eq!(r.status, 200);
    assert_eq!(
        header(&r, "docker-distribution-api-version"),
        Some("registry/2.0")
    );
}

#[test]
fn monolithic_blob_then_manifest_roundtrip() {
    let (_d, s) = store();
    let blob = b"hello layer".to_vec();
    let bd = digest::sha256(&blob);

    // Monolithic single-POST blob upload.
    let r = dispatch(
        &s,
        &req_body(
            "POST",
            &format!("/v2/demo/app/blobs/uploads/?digest={bd}"),
            "application/octet-stream",
            blob.clone(),
        ),
    );
    assert_eq!(r.status, 201, "monolithic upload should 201");
    assert_eq!(header(&r, "docker-content-digest"), Some(bd.as_str()));

    // HEAD + GET the blob.
    let r = dispatch(&s, &req("HEAD", &format!("/v2/demo/app/blobs/{bd}")));
    assert_eq!(r.status, 200);
    assert_eq!(
        header(&r, "content-length"),
        Some(blob.len().to_string().as_str())
    );

    let r = dispatch(&s, &req("GET", &format!("/v2/demo/app/blobs/{bd}")));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, blob);

    // Build a tiny image manifest referencing the blob as config.
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": bd, "size": blob.len() },
        "layers": []
    });
    let mbytes = serde_json::to_vec(&manifest).unwrap();
    let md = digest::sha256(&mbytes);

    let r = dispatch(
        &s,
        &req_body(
            "PUT",
            "/v2/demo/app/manifests/v1",
            "application/vnd.oci.image.manifest.v1+json",
            mbytes.clone(),
        ),
    );
    assert_eq!(r.status, 201, "manifest PUT should 201");
    assert_eq!(header(&r, "docker-content-digest"), Some(md.as_str()));

    // Pull by tag and by digest.
    let r = dispatch(&s, &req("GET", "/v2/demo/app/manifests/v1"));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, mbytes);
    assert_eq!(header(&r, "docker-content-digest"), Some(md.as_str()));

    let r = dispatch(&s, &req("GET", &format!("/v2/demo/app/manifests/{md}")));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, mbytes);

    // Catalog + tags.
    let r = dispatch(&s, &req("GET", "/v2/_catalog"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["repositories"], serde_json::json!(["demo/app"]));

    let r = dispatch(&s, &req("GET", "/v2/demo/app/tags/list"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["name"], "demo/app");
    assert_eq!(v["tags"], serde_json::json!(["v1"]));
}

#[test]
fn chunked_upload_flow() {
    let (_d, s) = store();
    let part1 = b"first-".to_vec();
    let part2 = b"second".to_vec();
    let full = [part1.clone(), part2.clone()].concat();
    let fd = digest::sha256(&full);

    // Start session.
    let r = dispatch(&s, &req("POST", "/v2/lib/x/blobs/uploads/"));
    assert_eq!(r.status, 202);
    let loc = header(&r, "location").unwrap().to_string();
    let uuid = header(&r, "docker-upload-uuid").unwrap().to_string();
    assert!(loc.contains(&uuid));

    // PATCH two chunks.
    let mut p = req("PATCH", &loc);
    p.body = part1.clone();
    let r = dispatch(&s, &p);
    assert_eq!(r.status, 202);
    assert_eq!(
        header(&r, "range"),
        Some(format!("0-{}", part1.len() - 1).as_str())
    );

    let mut p = req("PATCH", &loc);
    p.body = part2.clone();
    let r = dispatch(&s, &p);
    assert_eq!(r.status, 202);

    // GET upload status.
    let r = dispatch(&s, &req("GET", &loc));
    assert_eq!(r.status, 204);
    assert_eq!(
        header(&r, "range"),
        Some(format!("0-{}", full.len() - 1).as_str())
    );

    // PUT to finalize.
    let r = dispatch(&s, &req("PUT", &format!("{loc}?digest={fd}")));
    assert_eq!(r.status, 201);
    assert_eq!(header(&r, "docker-content-digest"), Some(fd.as_str()));

    // Blob is now pullable.
    let r = dispatch(&s, &req("GET", &format!("/v2/lib/x/blobs/{fd}")));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, full);
}

#[test]
fn digest_mismatch_is_rejected() {
    let (_d, s) = store();
    let wrong = format!("sha256:{}", "0".repeat(64));
    let r = dispatch(
        &s,
        &req_body(
            "POST",
            &format!("/v2/x/blobs/uploads/?digest={wrong}"),
            "application/octet-stream",
            b"content".to_vec(),
        ),
    );
    assert_eq!(r.status, 400);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["errors"][0]["code"], "DIGEST_INVALID");
}

#[test]
fn unknown_blob_and_manifest_404() {
    let (_d, s) = store();
    let d = format!("sha256:{}", "a".repeat(64));
    let r = dispatch(&s, &req("GET", &format!("/v2/x/blobs/{d}")));
    assert_eq!(r.status, 404);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["errors"][0]["code"], "BLOB_UNKNOWN");

    // Unknown repo => NAME_UNKNOWN on tags.
    let r = dispatch(&s, &req("GET", "/v2/nope/tags/list"));
    assert_eq!(r.status, 404);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["errors"][0]["code"], "NAME_UNKNOWN");
}

#[test]
fn invalid_name_rejected() {
    let (_d, s) = store();
    let r = dispatch(&s, &req("GET", "/v2/Bad_NAME/tags/list"));
    assert_eq!(r.status, 400);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["errors"][0]["code"], "NAME_INVALID");
}

#[test]
fn cross_repo_mount_when_blob_present() {
    let (_d, s) = store();
    let blob = b"shared".to_vec();
    let bd = s.write_blob(&blob).unwrap();
    let r = dispatch(
        &s,
        &req(
            "POST",
            &format!("/v2/other/repo/blobs/uploads/?mount={bd}&from=src/repo"),
        ),
    );
    assert_eq!(r.status, 201, "mount of existing blob should 201");
    assert_eq!(header(&r, "docker-content-digest"), Some(bd.as_str()));
}

#[test]
fn delete_manifest_and_blob() {
    let (_d, s) = store();
    let manifest = serde_json::json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]});
    let mbytes = serde_json::to_vec(&manifest).unwrap();
    let r = dispatch(
        &s,
        &req_body(
            "PUT",
            "/v2/d/m/manifests/t",
            "application/vnd.oci.image.manifest.v1+json",
            mbytes,
        ),
    );
    assert_eq!(r.status, 201);
    let md = header(&r, "docker-content-digest").unwrap().to_string();

    let r = dispatch(&s, &req("DELETE", &format!("/v2/d/m/manifests/{md}")));
    assert_eq!(r.status, 202);
    // Tag is gone too.
    let r = dispatch(&s, &req("GET", "/v2/d/m/manifests/t"));
    assert_eq!(r.status, 404);
}

#[test]
fn referrers_lists_manifests_with_subject() {
    let (_d, s) = store();
    // Subject manifest.
    let subj = serde_json::json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]});
    let sb = serde_json::to_vec(&subj).unwrap();
    let sd = digest::sha256(&sb);
    s.put_manifest(
        "r/p",
        &sb,
        "application/vnd.oci.image.manifest.v1+json",
        Some("base"),
    )
    .unwrap();

    // Referring artifact with subject + artifactType.
    let art = serde_json::json!({
        "schemaVersion":2,
        "mediaType":"application/vnd.oci.image.manifest.v1+json",
        "artifactType":"application/vnd.example.sbom",
        "subject": {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest": sd, "size": sb.len()},
        "layers":[]
    });
    let ab = serde_json::to_vec(&art).unwrap();
    s.put_manifest(
        "r/p",
        &ab,
        "application/vnd.oci.image.manifest.v1+json",
        None,
    )
    .unwrap();

    let r = dispatch(&s, &req("GET", &format!("/v2/r/p/referrers/{sd}")));
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["mediaType"], "application/vnd.oci.image.index.v1+json");
    assert_eq!(v["manifests"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["manifests"][0]["artifactType"],
        "application/vnd.example.sbom"
    );

    // artifactType filter that matches nothing.
    let r = dispatch(
        &s,
        &req("GET", &format!("/v2/r/p/referrers/{sd}?artifactType=other")),
    );
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["manifests"].as_array().unwrap().len(), 0);
    assert_eq!(header(&r, "oci-filters-applied"), Some("artifactType"));
}

#[test]
fn catalog_pagination() {
    let (_d, s) = store();
    for i in 0..5 {
        let m = serde_json::json!({"schemaVersion":2,"layers":[]});
        s.put_manifest(
            &format!("repo{i}"),
            &serde_json::to_vec(&m).unwrap(),
            "application/vnd.oci.image.manifest.v1+json",
            Some("t"),
        )
        .unwrap();
    }
    let r = dispatch(&s, &req("GET", "/v2/_catalog?n=2"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["repositories"].as_array().unwrap().len(), 2);
    let link = header(&r, "link").expect("should have next link");
    assert!(link.contains("last=repo1"), "link was {link}");
}

#[test]
fn browse_ui_and_health() {
    let (_d, s) = store();
    let r = dispatch(&s, &req("GET", "/healthz"));
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["status"], "ok");

    let r = dispatch(&s, &req("GET", "/"));
    assert_eq!(r.status, 200);
    assert!(header(&r, "content-type").unwrap().contains("text/html"));

    let r = dispatch(&s, &req("GET", "/api/repositories"));
    assert_eq!(r.status, 200);
}
