//! Content-addressed disk storage for the registry.
//!
//! Everything lives under a single root (the `/data` volume in production):
//!
//! ```text
//! <root>/
//!   blobs/sha256/<hex>                                  content-addressed blobs
//!   uploads/<id>                                        in-progress upload sessions
//!   repos/<name>/_manifests/revisions/sha256/<hex>      manifest bytes
//!   repos/<name>/_manifests/revisions/sha256/<hex>.mt   manifest media type
//!   repos/<name>/_manifests/tags/<tag>                  digest the tag points at
//! ```
//!
//! Blobs are global and shared across repositories (standard registry
//! behaviour); manifests and tags are per-repository. There is no in-memory
//! state — each request reads and writes the filesystem directly, which is
//! what lets the component scale to zero between requests.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::digest;

/// Process-wide counter to keep upload ids unique within a single instance,
/// combined with the wall clock to stay unique across instances.
static UPLOAD_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct Storage {
    root: PathBuf,
}

/// Outcome of finalizing an upload.
#[derive(Debug)]
pub enum FinalizeError {
    /// The session id does not exist.
    UnknownUpload,
    /// The computed digest did not match the client-supplied one.
    DigestMismatch {
        expected: String,
        got: String,
    },
    /// The supplied digest was not a valid `algorithm:hex` string.
    InvalidDigest,
    Io(io::Error),
}

impl From<io::Error> for FinalizeError {
    fn from(e: io::Error) -> Self {
        FinalizeError::Io(e)
    }
}

/// A stored manifest: raw bytes plus the media type it was pushed with.
pub struct StoredManifest {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub digest: String,
}

impl Storage {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ----- blobs -----------------------------------------------------------

    fn blob_path(&self, dgst: &str) -> Option<PathBuf> {
        let (algo, hex) = digest::parse(dgst)?;
        Some(self.root.join("blobs").join(algo).join(hex))
    }

    pub fn blob_exists(&self, dgst: &str) -> bool {
        self.blob_path(dgst).map(|p| p.is_file()).unwrap_or(false)
    }

    pub fn blob_size(&self, dgst: &str) -> Option<u64> {
        let p = self.blob_path(dgst)?;
        fs::metadata(p).ok().map(|m| m.len())
    }

    pub fn read_blob(&self, dgst: &str) -> Option<Vec<u8>> {
        let p = self.blob_path(dgst)?;
        fs::read(p).ok()
    }

    /// Write `bytes` as a blob, keyed by its own sha256. Returns the digest.
    pub fn write_blob(&self, bytes: &[u8]) -> io::Result<String> {
        let dgst = digest::sha256(bytes);
        let path = self
            .blob_path(&dgst)
            .expect("freshly computed sha256 is always valid");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.is_file() {
            atomic_write(&path, bytes)?;
        }
        Ok(dgst)
    }

    pub fn delete_blob(&self, dgst: &str) -> io::Result<bool> {
        match self.blob_path(dgst) {
            Some(p) if p.is_file() => {
                fs::remove_file(p)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    // ----- uploads ---------------------------------------------------------

    fn upload_path(&self, id: &str) -> PathBuf {
        self.root.join("uploads").join(id)
    }

    /// Begin a new upload session, returning its id.
    pub fn create_upload(&self) -> io::Result<String> {
        let dir = self.root.join("uploads");
        fs::create_dir_all(&dir)?;
        let id = new_upload_id();
        // Create an empty session file so status/append have something to find.
        fs::File::create(dir.join(&id))?;
        Ok(id)
    }

    pub fn upload_exists(&self, id: &str) -> bool {
        is_safe_id(id) && self.upload_path(id).is_file()
    }

    /// Current number of bytes received for the session, if it exists.
    pub fn upload_size(&self, id: &str) -> Option<u64> {
        if !is_safe_id(id) {
            return None;
        }
        fs::metadata(self.upload_path(id)).ok().map(|m| m.len())
    }

    /// Append `chunk` to the session. Returns the new total length.
    pub fn append_upload(&self, id: &str, chunk: &[u8]) -> io::Result<u64> {
        if !is_safe_id(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad upload id"));
        }
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.upload_path(id))?;
        f.write_all(chunk)?;
        f.flush()?;
        Ok(fs::metadata(self.upload_path(id))?.len())
    }

    pub fn cancel_upload(&self, id: &str) -> io::Result<bool> {
        if !is_safe_id(id) {
            return Ok(false);
        }
        let p = self.upload_path(id);
        if p.is_file() {
            fs::remove_file(p)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Optionally append a final `chunk`, then verify the assembled bytes
    /// hash to `expected_digest` and promote the session to a blob.
    pub fn finalize_upload(
        &self,
        id: &str,
        chunk: &[u8],
        expected_digest: &str,
    ) -> Result<String, FinalizeError> {
        if !is_safe_id(id) || !self.upload_path(id).is_file() {
            return Err(FinalizeError::UnknownUpload);
        }
        if !digest::is_valid(expected_digest) {
            return Err(FinalizeError::InvalidDigest);
        }
        if !chunk.is_empty() {
            self.append_upload(id, chunk)?;
        }
        let bytes = fs::read(self.upload_path(id))?;
        let got = digest::sha256(&bytes);
        if got != expected_digest {
            return Err(FinalizeError::DigestMismatch {
                expected: expected_digest.to_string(),
                got,
            });
        }
        self.write_blob(&bytes)?;
        let _ = fs::remove_file(self.upload_path(id));
        Ok(got)
    }

    // ----- manifests -------------------------------------------------------

    fn repo_dir(&self, name: &str) -> PathBuf {
        self.root.join("repos").join(name)
    }

    fn manifest_revision_path(&self, name: &str, dgst: &str) -> Option<PathBuf> {
        let (algo, hex) = digest::parse(dgst)?;
        Some(
            self.repo_dir(name)
                .join("_manifests")
                .join("revisions")
                .join(algo)
                .join(hex),
        )
    }

    fn tag_path(&self, name: &str, tag: &str) -> PathBuf {
        self.repo_dir(name)
            .join("_manifests")
            .join("tags")
            .join(tag)
    }

    /// Store a manifest under its digest and (optionally) point a tag at it.
    /// Returns the manifest digest.
    pub fn put_manifest(
        &self,
        name: &str,
        bytes: &[u8],
        media_type: &str,
        tag: Option<&str>,
    ) -> io::Result<String> {
        let dgst = digest::sha256(bytes);
        let path = self
            .manifest_revision_path(name, &dgst)
            .expect("freshly computed sha256 is always valid");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, bytes)?;
        atomic_write(&path.with_extension("mt"), media_type.as_bytes())?;
        if let Some(tag) = tag {
            let tp = self.tag_path(name, tag);
            if let Some(parent) = tp.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_write(&tp, dgst.as_bytes())?;
        }
        Ok(dgst)
    }

    /// Resolve a reference (a tag or a digest) to the manifest digest.
    pub fn resolve_reference(&self, name: &str, reference: &str) -> Option<String> {
        if digest::is_valid(reference) {
            if self
                .manifest_revision_path(name, reference)
                .map(|p| p.is_file())
                .unwrap_or(false)
            {
                return Some(reference.to_string());
            }
            return None;
        }
        // It's a tag.
        let tp = self.tag_path(name, reference);
        fs::read_to_string(tp).ok().map(|s| s.trim().to_string())
    }

    pub fn get_manifest(&self, name: &str, reference: &str) -> Option<StoredManifest> {
        let dgst = self.resolve_reference(name, reference)?;
        let path = self.manifest_revision_path(name, &dgst)?;
        let bytes = fs::read(&path).ok()?;
        let media_type = fs::read_to_string(path.with_extension("mt"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| guess_media_type(&bytes));
        Some(StoredManifest {
            bytes,
            media_type,
            digest: dgst,
        })
    }

    /// Delete a manifest revision and any tags pointing at it. Returns true
    /// if the revision existed.
    pub fn delete_manifest(&self, name: &str, dgst: &str) -> io::Result<bool> {
        let Some(path) = self.manifest_revision_path(name, dgst) else {
            return Ok(false);
        };
        if !path.is_file() {
            return Ok(false);
        }
        fs::remove_file(&path)?;
        let _ = fs::remove_file(path.with_extension("mt"));
        // Drop dangling tags.
        let tags_dir = self.repo_dir(name).join("_manifests").join("tags");
        if let Ok(entries) = fs::read_dir(&tags_dir) {
            for e in entries.flatten() {
                if let Ok(target) = fs::read_to_string(e.path()) {
                    if target.trim() == dgst {
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }
        Ok(true)
    }

    /// Delete a single tag (the manifest revision is left intact). Returns
    /// true if the tag existed.
    pub fn delete_tag(&self, name: &str, tag: &str) -> io::Result<bool> {
        let tp = self.tag_path(name, tag);
        if tp.is_file() {
            fs::remove_file(tp)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// True if the repository exists (has a `_manifests` directory).
    pub fn repo_exists(&self, name: &str) -> bool {
        self.repo_dir(name).join("_manifests").is_dir()
    }

    /// All tags in a repository, sorted.
    pub fn list_tags(&self, name: &str) -> Vec<String> {
        let dir = self.repo_dir(name).join("_manifests").join("tags");
        let mut tags: Vec<String> = match fs::read_dir(dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        tags.sort();
        tags
    }

    /// All repository names (any directory under `repos/` containing a
    /// `_manifests` dir), sorted.
    pub fn list_repositories(&self) -> Vec<String> {
        let mut out = Vec::new();
        let base = self.root.join("repos");
        walk_repos(&base, &base, &mut out);
        out.sort();
        out
    }

    /// Digests of all manifest revisions in a repository.
    pub fn list_manifest_digests(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        let base = self.repo_dir(name).join("_manifests").join("revisions");
        for algo in ["sha256", "sha512"] {
            let dir = base.join(algo);
            if let Ok(entries) = fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_file() && p.extension().is_none() {
                        if let Some(hex) = p.file_name().and_then(|n| n.to_str()) {
                            out.push(format!("{algo}:{hex}"));
                        }
                    }
                }
            }
        }
        out
    }
}

/// Recursively collect repository names: any directory containing a
/// `_manifests` subdir, recorded as its path relative to `base`.
fn walk_repos(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let name = e.file_name();
        if name == "_manifests" {
            if let Ok(rel) = dir.strip_prefix(base) {
                if let Some(s) = rel.to_str() {
                    if !s.is_empty() {
                        out.push(s.replace('\\', "/"));
                    }
                }
            }
            continue; // don't descend into _manifests
        }
        walk_repos(base, &p, out);
    }
}

/// Best-effort media-type sniff for manifests stored without a sidecar.
fn guess_media_type(bytes: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if let Some(mt) = v.get("mediaType").and_then(|m| m.as_str()) {
            return mt.to_string();
        }
    }
    "application/vnd.oci.image.manifest.v1+json".to_string()
}

/// Write `bytes` to `path` atomically via a temp file + rename.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Upload ids are derived from the wall clock and a process counter, hashed
/// to a hex string. Unique enough for local dev; never user-controlled.
fn new_upload_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = UPLOAD_SEQ.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{nanos}-{seq}");
    // Reuse sha256 and take the first 32 hex chars as a compact opaque id.
    let h = digest::sha256(seed.as_bytes());
    h.trim_start_matches("sha256:")[..32].to_string()
}

/// An upload id we generated is 32 lowercase hex chars. Reject anything else
/// to keep it from escaping the uploads directory.
fn is_safe_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
