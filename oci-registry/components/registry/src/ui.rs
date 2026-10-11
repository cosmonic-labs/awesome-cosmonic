//! Local-development browse surface: a tiny single-page UI plus the JSON it
//! reads. None of this is part of the OCI spec — it's here so `wash dev`
//! gives you something to look at while pushing images.

use crate::http::{Method, Request, Response};
use crate::storage::Storage;

/// JSON: every repository with its tag count.
pub fn api_repositories(store: &Storage) -> Response {
    let repos: Vec<serde_json::Value> = store
        .list_repositories()
        .into_iter()
        .map(|name| {
            let tags = store.list_tags(&name);
            let manifests = store.list_manifest_digests(&name).len();
            serde_json::json!({
                "name": name,
                "tags": tags.len(),
                "manifests": manifests,
            })
        })
        .collect();
    cors(Response::json(
        200,
        serde_json::json!({ "repositories": repos }),
    ))
}

/// JSON: one repository's tags, each resolved to a digest + total size.
pub fn api_repository(store: &Storage, name: &str) -> Response {
    if !store.repo_exists(name) {
        return cors(Response::json(
            404,
            serde_json::json!({ "error": "repository not found", "name": name }),
        ));
    }
    let tags: Vec<serde_json::Value> = store
        .list_tags(name)
        .into_iter()
        .map(|tag| {
            let digest = store.resolve_reference(name, &tag);
            let (size, media_type) = digest
                .as_deref()
                .and_then(|d| store.get_manifest(name, d))
                .map(|m| (manifest_total_size(store, &m.bytes), m.media_type))
                .unwrap_or((0, String::new()));
            serde_json::json!({
                "tag": tag,
                "digest": digest,
                "size": size,
                "mediaType": media_type,
            })
        })
        .collect();
    cors(Response::json(
        200,
        serde_json::json!({
            "name": name,
            "tags": tags,
            "manifests": store.list_manifest_digests(name),
        }),
    ))
}

/// Total addressable size of a manifest: the manifest itself, its config, and
/// all layers (best effort — unknown shapes return the manifest size only).
fn manifest_total_size(store: &Storage, bytes: &[u8]) -> u64 {
    let mut total = bytes.len() as u64;
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if let Some(cfg) = v
            .get("config")
            .and_then(|c| c.get("digest"))
            .and_then(|d| d.as_str())
        {
            total += store.blob_size(cfg).unwrap_or(0);
        }
        if let Some(layers) = v.get("layers").and_then(|l| l.as_array()) {
            for layer in layers {
                if let Some(d) = layer.get("digest").and_then(|d| d.as_str()) {
                    total += store.blob_size(d).unwrap_or(0);
                }
            }
        }
    }
    total
}

/// The HTML browse page (everything inline, no external assets).
pub fn index(store: &Storage, req: &Request) -> Response {
    if req.method != Method::Get && req.method != Method::Head {
        return Response::error(crate::error::ErrorCode::Unsupported, "method not allowed");
    }
    let count = store.list_repositories().len();
    let host = req.host.clone().unwrap_or_else(|| "localhost:8080".into());
    let html = PAGE
        .replace("{{COUNT}}", &count.to_string())
        .replace("{{HOST}}", &html_escape(&host));
    let mut resp = Response::html(200, html);
    if req.method == Method::Head {
        resp.body.clear();
    }
    resp
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cors(resp: Response) -> Response {
    resp.with_header("access-control-allow-origin", "*")
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OCI Registry</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0; font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
    background: #0b0e14; color: #c5cdd9;
  }
  header {
    padding: 20px 24px; border-bottom: 1px solid #1c2230;
    display: flex; align-items: baseline; gap: 12px;
  }
  header h1 { margin: 0; font-size: 16px; color: #e6edf3; letter-spacing: .5px; }
  header .sub { color: #5b6675; font-size: 12px; }
  .wrap { max-width: 980px; margin: 0 auto; padding: 24px; }
  .card {
    border: 1px solid #1c2230; border-radius: 8px; background: #0f131c;
    margin-bottom: 12px; overflow: hidden;
  }
  .repo-head {
    padding: 12px 16px; cursor: pointer; display: flex; justify-content: space-between;
    align-items: center; gap: 12px;
  }
  .repo-head:hover { background: #131927; }
  .repo-name { color: #6cb6ff; font-weight: 600; }
  .badge {
    color: #768390; font-size: 12px; border: 1px solid #1c2230;
    border-radius: 999px; padding: 1px 8px;
  }
  .tags { border-top: 1px solid #1c2230; display: none; }
  .tags.open { display: block; }
  table { width: 100%; border-collapse: collapse; }
  td, th { text-align: left; padding: 8px 16px; border-bottom: 1px solid #161c28; font-size: 13px; }
  th { color: #5b6675; font-weight: 500; }
  .tag { color: #e6edf3; }
  .digest { color: #768390; font-size: 12px; }
  .pull { color: #57ab5a; font-size: 12px; }
  .empty { color: #5b6675; padding: 40px; text-align: center; }
  code { background: #161c28; padding: 1px 5px; border-radius: 4px; color: #adbac7; }
  a { color: #6cb6ff; text-decoration: none; }
</style>
</head>
<body>
<header>
  <h1>OCI Registry</h1>
  <span class="sub">wasmCloud v2 · scale-to-zero · {{COUNT}} repositories</span>
</header>
<div class="wrap">
  <p class="sub">Push with <code>wash oci push {{HOST}}/myimage:tag ./component.wasm</code>
     or any OCI client (<code>oras</code>, <code>docker</code>, <code>crane</code>).</p>
  <div id="list"><div class="empty">loading…</div></div>
</div>
<script>
const HOST = "{{HOST}}";
function fmtBytes(n) {
  if (!n) return "0 B";
  const u = ["B","KB","MB","GB"]; let i = 0;
  while (n >= 1024 && i < u.length-1) { n /= 1024; i++; }
  return n.toFixed(i ? 1 : 0) + " " + u[i];
}
async function load() {
  const list = document.getElementById("list");
  const r = await fetch("/api/repositories");
  const d = await r.json();
  if (!d.repositories.length) {
    list.innerHTML = '<div class="empty">No repositories yet. Push an image to get started.</div>';
    return;
  }
  list.innerHTML = "";
  for (const repo of d.repositories) {
    const card = document.createElement("div");
    card.className = "card";
    card.innerHTML = `
      <div class="repo-head">
        <span class="repo-name">${repo.name}</span>
        <span class="badge">${repo.tags} tags · ${repo.manifests} manifests</span>
      </div>
      <div class="tags"></div>`;
    const head = card.querySelector(".repo-head");
    const tagsEl = card.querySelector(".tags");
    head.addEventListener("click", async () => {
      tagsEl.classList.toggle("open");
      if (tagsEl.dataset.loaded) return;
      const rr = await fetch("/api/repository/" + repo.name);
      const dd = await rr.json();
      tagsEl.dataset.loaded = "1";
      if (!dd.tags.length) { tagsEl.innerHTML = '<div class="empty">No tags.</div>'; return; }
      let rows = dd.tags.map(t => `
        <tr>
          <td class="tag">${t.tag}</td>
          <td class="digest">${(t.digest||"").slice(0,19)}…</td>
          <td>${fmtBytes(t.size)}</td>
          <td class="pull">${HOST}/${repo.name}:${t.tag}</td>
        </tr>`).join("");
      tagsEl.innerHTML = `<table>
        <tr><th>tag</th><th>digest</th><th>size</th><th>pull</th></tr>${rows}</table>`;
    });
    list.appendChild(card);
  }
}
load();
</script>
</body>
</html>"#;
