//! Filesystem operations behind the tools — a port of the official
//! `@modelcontextprotocol/server-filesystem` reference server's `lib.ts`
//! (MIT) onto `std::fs` over WASI preopens.
//!
//! Everything the tools touch goes through [`validate_path`]: a lexical
//! normalisation, the allowed-directory prefix check, and symlink resolution
//! through `std::fs::canonicalize` (which the WASI runtime restricts to the
//! preopened tree — an escaping symlink comes back as `PermissionDenied`, the
//! host's errno 63 "Operation not permitted"). The WASI preopen jail is the
//! real security boundary; the checks here exist so the caller gets the
//! reference server's error strings instead of a raw errno.
//!
//! Nothing in this module caches directory listings or file contents: the
//! host folder is a two-way door shared with the user, and a second client
//! must never see stale data. Only [`Config`] is parsed per call.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

/// Default for `FS_MAX_FILE_BYTES`: the cap on bytes read per file.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1_048_576;
/// Hard clamp range for `FS_MAX_FILE_BYTES`.
pub const MIN_MAX_FILE_BYTES: u64 = 1024;
pub const MAX_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Default for `FS_MAX_RESULTS`: entries returned by listings/searches/trees.
pub const DEFAULT_MAX_RESULTS: usize = 1000;
pub const MAX_MAX_RESULTS: usize = 100_000;
/// Default for `FS_MAX_TREE_DEPTH`: recursion depth for tree/search.
pub const DEFAULT_MAX_TREE_DEPTH: usize = 10;
pub const MAX_TREE_DEPTH_CEILING: usize = 64;
/// Largest `content` accepted by `write_file` (the transport's request-body
/// cap is far lower; this is the in-guest bound).
pub const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024;
/// Most paths one `read_multiple_files` call processes.
pub const MAX_PATHS_PER_CALL: usize = 100;
/// Most edits one `edit_file` call applies.
pub const MAX_EDITS_PER_CALL: usize = 200;
/// Largest `head`/`tail` line count honoured.
pub const MAX_HEAD_TAIL_LINES: u64 = 1_000_000;
/// Longest guest path accepted (bytes).
pub const MAX_PATH_BYTES: usize = 4096;
/// Read chunk for the streaming head/tail readers.
const CHUNK: usize = 8192;

/// The actionable error every tool returns when the allow-list is missing.
pub const MISSING_CONFIG: &str = "FS_ALLOWED_DIRS is not set. Mount a host folder with \
    spec.volumes (hostPath) + localResources.volumeMounts (mountPath) in deploy/workload.yaml \
    and set FS_ALLOWED_DIRS to the mountPath(s), e.g. /data.";

/// A tool-level failure: the message is written for the calling agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsError(pub String);

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for FsError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for FsError {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}

/// Server configuration, read from the environment on every call.
#[derive(Debug, Clone)]
pub struct Config {
    /// Normalised absolute guest paths the tools may touch, in the order
    /// given (the first one is where relative paths resolve).
    pub allowed: Vec<String>,
    /// `FS_READ_ONLY`: every mutating tool refuses.
    pub read_only: bool,
    /// `FS_MAX_FILE_BYTES`.
    pub max_file_bytes: u64,
    /// `FS_MAX_RESULTS`.
    pub max_results: usize,
    /// `FS_MAX_TREE_DEPTH`.
    pub max_tree_depth: usize,
}

impl Config {
    /// Parses the environment. `FS_ALLOWED_DIRS` is required; the numeric
    /// settings fall back to their defaults on absence or garbage and are
    /// clamped to their documented ranges.
    pub fn from_env() -> Result<Self, FsError> {
        let raw = std::env::var("FS_ALLOWED_DIRS").unwrap_or_default();
        let mut allowed = Vec::new();
        for entry in raw.split(',') {
            let entry = strip_quotes(entry.trim());
            if entry.is_empty() {
                continue;
            }
            if !entry.starts_with('/') {
                return Err(FsError(format!(
                    "FS_ALLOWED_DIRS entry '{entry}' is not an absolute guest path; \
                     use the volumeMounts mountPath, e.g. /data"
                )));
            }
            if entry.contains('\0') {
                return Err(FsError(
                    "FS_ALLOWED_DIRS contains a NUL byte; use plain guest paths like /data".into(),
                ));
            }
            let normalised = normalize_absolute(entry);
            if !allowed.contains(&normalised) {
                allowed.push(normalised);
            }
        }
        if allowed.is_empty() {
            return Err(FsError(MISSING_CONFIG.to_owned()));
        }

        let read_only = std::env::var("FS_READ_ONLY")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "true" || v == "1" || v == "yes"
            })
            .unwrap_or(false);

        let max_file_bytes = env_u64("FS_MAX_FILE_BYTES", DEFAULT_MAX_FILE_BYTES)
            .clamp(MIN_MAX_FILE_BYTES, MAX_MAX_FILE_BYTES);
        let max_results = (env_u64("FS_MAX_RESULTS", DEFAULT_MAX_RESULTS as u64) as usize)
            .clamp(1, MAX_MAX_RESULTS);
        let max_tree_depth = (env_u64("FS_MAX_TREE_DEPTH", DEFAULT_MAX_TREE_DEPTH as u64) as usize)
            .clamp(1, MAX_TREE_DEPTH_CEILING);

        Ok(Self {
            allowed,
            read_only,
            max_file_bytes,
            max_results,
            max_tree_depth,
        })
    }

    /// The allowed directories joined the way the reference server prints
    /// them in its "Access denied" messages.
    pub fn allowed_list(&self) -> String {
        self.allowed.join(", ")
    }

    /// Whether a normalised absolute path is inside one of the allowed
    /// directories (the directory itself counts).
    pub fn is_within(&self, path: &str) -> bool {
        self.allowed.iter().any(|dir| {
            if dir == "/" {
                path.starts_with('/')
            } else {
                path == dir
                    || path
                        .strip_prefix(dir.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }
        })
    }

    /// Whether `dir` is actually reachable (mounted) in this instance.
    pub fn is_mounted(dir: &str) -> bool {
        fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false)
    }

    /// Clamps a caller-supplied depth to `1..=max_tree_depth`.
    pub fn clamp_depth(&self, requested: Option<u64>) -> usize {
        match requested {
            Some(depth) => (depth.min(self.max_tree_depth as u64).max(1)) as usize,
            None => self.max_tree_depth,
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn strip_quotes(s: &str) -> &str {
    let s = s
        .strip_prefix('"')
        .or_else(|| s.strip_prefix('\''))
        .unwrap_or(s);
    s.strip_suffix('"')
        .or_else(|| s.strip_suffix('\''))
        .unwrap_or(s)
}

/// Lexically normalises an absolute POSIX path: collapses `//`, drops `.`,
/// resolves `..` (never above `/`), strips the trailing slash.
pub fn normalize_absolute(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
    }
}

fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// How [`validate_path`] treats a path that does not exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// The path must exist (reads, listings, move source).
    Deny,
    /// The final component may be missing but its parent must exist
    /// (`write_file`, `move_file` destination).
    AllowLeaf,
    /// Any number of trailing components may be missing as long as the
    /// nearest existing ancestor is allowed (`create_directory`).
    AllowAncestors,
}

/// A validated path: what the caller asked for, and what to operate on.
#[derive(Debug, Clone)]
pub struct Validated {
    /// Lexically normalised absolute guest path (symlinks unresolved).
    pub requested: String,
    /// Symlink-resolved path to operate on. For a path that does not exist
    /// yet this is its canonical existing ancestor plus the missing tail.
    pub real: PathBuf,
    /// Whether `requested` exists right now.
    pub exists: bool,
}

/// Port of the reference server's `validatePath`: normalise, resolve relative
/// paths against the allowed directories, require the result to lie inside
/// one of them, then resolve symlinks and require the target to as well.
pub fn validate_path(
    cfg: &Config,
    requested: &str,
    missing: Missing,
) -> Result<Validated, FsError> {
    let raw = strip_quotes(requested.trim());
    if raw.is_empty() {
        return Err(FsError("path must not be empty".into()));
    }
    if raw.len() > MAX_PATH_BYTES {
        return Err(FsError(format!(
            "path is too long ({} bytes; max {MAX_PATH_BYTES})",
            raw.len()
        )));
    }
    if raw.contains('\0') {
        return Err(FsError(format!(
            "Access denied - path contains a NUL byte: {}",
            raw.replace('\0', "\\0")
        )));
    }
    if is_windows_path(raw) {
        return Err(FsError(format!(
            "Access denied - Windows-style path received on a POSIX host: {raw}"
        )));
    }

    let absolute = if raw.starts_with('/') {
        normalize_absolute(raw)
    } else {
        resolve_relative(cfg, raw)
    };

    if !cfg.is_within(&absolute) {
        return Err(FsError(format!(
            "Access denied - path outside allowed directories: {absolute} not in {}",
            cfg.allowed_list()
        )));
    }

    match fs::canonicalize(&absolute) {
        Ok(real) => {
            let real_str = real.to_string_lossy();
            if !cfg.is_within(&real_str) {
                return Err(FsError(format!(
                    "Access denied - symlink target outside allowed directories: {real_str} not in {}",
                    cfg.allowed_list()
                )));
            }
            Ok(Validated {
                requested: absolute,
                real,
                exists: true,
            })
        }
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            Err(symlink_denied(cfg, &absolute))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            resolve_missing(cfg, absolute, missing)
        }
        Err(err) => Err(FsError(format!("cannot resolve {absolute}: {err}"))),
    }
}

fn resolve_missing(cfg: &Config, absolute: String, missing: Missing) -> Result<Validated, FsError> {
    let parent = parent_of(&absolute);
    match missing {
        Missing::Deny => Err(FsError(format!("No such file or directory: {absolute}"))),
        Missing::AllowLeaf => match fs::canonicalize(&parent) {
            Ok(real_parent) => {
                check_real_within(cfg, &real_parent)?;
                Ok(Validated {
                    real: real_parent.join(file_name_of(&absolute)),
                    requested: absolute,
                    exists: false,
                })
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(FsError(format!(
                "Parent directory does not exist: {parent}"
            ))),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                Err(symlink_denied(cfg, &parent))
            }
            Err(err) => Err(FsError(format!("cannot resolve {parent}: {err}"))),
        },
        Missing::AllowAncestors => {
            let mut ancestor = parent;
            let mut tail = vec![file_name_of(&absolute).to_owned()];
            loop {
                match fs::canonicalize(&ancestor) {
                    Ok(real_ancestor) => {
                        check_real_within(cfg, &real_ancestor)?;
                        let mut real = real_ancestor;
                        for component in tail.iter().rev() {
                            real.push(component);
                        }
                        return Ok(Validated {
                            requested: absolute,
                            real,
                            exists: false,
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        tail.push(file_name_of(&ancestor).to_owned());
                        ancestor = parent_of(&ancestor);
                        // Climbing above the allowed directory means the
                        // allowed directory itself is missing (not mounted).
                        if !cfg.is_within(&ancestor) {
                            return Err(FsError(format!(
                                "Parent directory does not exist: {}",
                                parent_of(&absolute)
                            )));
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                        return Err(symlink_denied(cfg, &ancestor));
                    }
                    Err(err) => return Err(FsError(format!("cannot resolve {ancestor}: {err}"))),
                }
            }
        }
    }
}

fn check_real_within(cfg: &Config, real: &Path) -> Result<(), FsError> {
    let real_str = real.to_string_lossy();
    if cfg.is_within(&real_str) {
        Ok(())
    } else {
        Err(FsError(format!(
            "Access denied - symlink target outside allowed directories: {real_str} not in {}",
            cfg.allowed_list()
        )))
    }
}

/// Builds the "symlink target outside allowed directories" message for a path
/// the runtime refused to resolve, naming the offending link and — when the
/// link target is readable — where it points.
fn symlink_denied(cfg: &Config, absolute: &str) -> FsError {
    let dirs = cfg.allowed_list();
    let mut prefix = String::new();
    for component in absolute.split('/').filter(|c| !c.is_empty()) {
        prefix.push('/');
        prefix.push_str(component);
        match fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return match fs::read_link(&prefix) {
                    Ok(target) => {
                        let target = target.to_string_lossy();
                        let resolved = if target.starts_with('/') {
                            normalize_absolute(&target)
                        } else {
                            normalize_absolute(&format!("{}/{}", parent_of(&prefix), target))
                        };
                        if cfg.is_within(&resolved) {
                            // The link text stays inside, but the runtime still
                            // refused: keep walking to find the real culprit.
                            continue;
                        }
                        FsError(format!(
                            "Access denied - symlink target outside allowed directories: {resolved} not in {dirs}"
                        ))
                    }
                    Err(_) => FsError(format!(
                        "Access denied - symlink target outside allowed directories: {prefix} points to an \
                         absolute or escaping target (not in {dirs})"
                    )),
                };
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => break,
            Err(_) => break,
        }
    }
    FsError(format!(
        "Access denied - the sandbox refused {absolute} (Operation not permitted): a path component \
         escapes the mounted folder (not in {dirs})"
    ))
}

fn resolve_relative(cfg: &Config, relative: &str) -> String {
    for dir in &cfg.allowed {
        let candidate = normalize_absolute(&format!("{dir}/{relative}"));
        if cfg.is_within(&candidate) {
            return candidate;
        }
    }
    let first = cfg.allowed.first().map(String::as_str).unwrap_or("/");
    normalize_absolute(&format!("{first}/{relative}"))
}

fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Renders an `io::Error` for a tool result: the operation, the path, and
/// the reason — with the WASI sandbox's errno 63 explained.
pub fn io_message(op: &str, path: &str, err: &io::Error) -> String {
    if err.kind() == io::ErrorKind::PermissionDenied {
        return format!(
            "{op} {path}: Operation not permitted (the sandbox refused to follow a symlink or \
             path outside the mounted folder)"
        );
    }
    format!("{op} {path}: {err}")
}

/// Human-readable size, as the reference server's `formatSize`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_owned();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Largest index `<= limit` on a UTF-8 boundary of `bytes` (so a cut there is
/// always valid text).
fn utf8_cut(bytes: &[u8], limit: usize) -> usize {
    let mut cut = limit.min(bytes.len());
    while cut > 0 && cut < bytes.len() && (bytes[cut] & 0xC0) == 0x80 {
        cut -= 1;
    }
    cut
}

/// Outcome of a capped text read.
#[derive(Debug)]
pub struct TextRead {
    pub text: String,
    /// `Some(total_size)` when the file was larger than the cap.
    pub truncated: Option<u64>,
    /// The bytes were not valid UTF-8 and were decoded lossily.
    pub lossy: bool,
}

/// Reads at most `cap` bytes of a file as text, cutting on a character
/// boundary and reporting whether more remained.
pub fn read_text_capped(path: &Path, cap: u64) -> io::Result<TextRead> {
    let mut file = fs::File::open(path)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(size.min(cap).saturating_add(1) as usize);
    (&mut file)
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let truncated = if bytes.len() as u64 > cap {
        let cut = utf8_cut(&bytes, cap as usize);
        bytes.truncate(cut);
        Some(size.max(cap + 1))
    } else {
        None
    };
    let (text, lossy) = decode(bytes);
    Ok(TextRead {
        text,
        truncated,
        lossy,
    })
}

fn decode(bytes: Vec<u8>) -> (String, bool) {
    match String::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), true),
    }
}

/// Appends the reference-style truncation/lossy notes to a capped read.
pub fn annotate_text(read: TextRead, cap: u64) -> String {
    let mut text = read.text;
    if read.lossy {
        text.push_str(
            "\n[note: file is not valid UTF-8; undecodable bytes were replaced with U+FFFD]",
        );
    }
    if let Some(size) = read.truncated {
        text.push_str(&format!(
            "\n[truncated: file is {size} bytes, cap is {cap}; use head/tail to read a range]"
        ));
    }
    text
}

/// Splits text into lines the way the reference server's head/tail readers
/// do, without counting a final line terminator as an empty line.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// The first `n` lines, read streaming (stops after `n` newlines or `cap`
/// bytes, whichever comes first).
pub fn read_head(path: &Path, n: u64, cap: u64) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk = [0u8; CHUNK];
    let mut newlines: u64 = 0;
    let mut hit_cap = false;
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        newlines += chunk[..read].iter().filter(|&&b| b == b'\n').count() as u64;
        bytes.extend_from_slice(&chunk[..read]);
        if newlines >= n {
            break;
        }
        if bytes.len() as u64 >= cap {
            hit_cap = true;
            break;
        }
    }
    if hit_cap {
        let cut = utf8_cut(&bytes, cap as usize);
        bytes.truncate(cut);
    }
    let (text, _) = decode(bytes);
    let lines = split_lines(&text);
    let mut out = lines
        .iter()
        .take(n as usize)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if hit_cap && (lines.len() as u64) < n {
        out.push_str(&format!(
            "\n[truncated: reached the FS_MAX_FILE_BYTES cap of {cap} bytes before {n} lines]"
        ));
    }
    Ok(out)
}

/// The last `n` lines, read backwards from the end in chunks (never more
/// than `cap` bytes).
pub fn read_tail(path: &Path, n: u64, cap: u64) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(String::new());
    }
    let mut position = size;
    let mut newlines: u64 = 0;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total: u64 = 0;
    let mut hit_cap = false;
    // One newline more than `n` guarantees `n` complete lines when the file
    // ends with a terminator.
    while position > 0 && newlines <= n {
        let read_size = (CHUNK as u64).min(position);
        position -= read_size;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0u8; read_size as usize];
        file.read_exact(&mut chunk)?;
        newlines += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        total += read_size;
        chunks.push(chunk);
        if total >= cap {
            hit_cap = true;
            break;
        }
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    for chunk in chunks.iter().rev() {
        bytes.extend_from_slice(chunk);
    }
    // Cutting at a chunk boundary can land mid-character: drop the leading
    // continuation bytes so the decode is clean.
    let start = bytes
        .iter()
        .position(|&b| (b & 0xC0) != 0x80)
        .unwrap_or(bytes.len());
    let (text, _) = decode(bytes[start..].to_vec());
    let lines = split_lines(&text);
    let skip = lines.len().saturating_sub(n as usize);
    let mut out = lines[skip..].join("\n");
    if hit_cap && (lines.len() as u64) < n {
        out.insert_str(
            0,
            &format!(
                "[truncated: reached the FS_MAX_FILE_BYTES cap of {cap} bytes before {n} lines]\n"
            ),
        );
    }
    Ok(out)
}

/// Writes `content` without partial results: exclusive create first (which
/// refuses to write through an existing symlink), else a temp file in the
/// same directory renamed over the target — the reference server's
/// `writeFileContent`.
pub fn write_atomic(real: &Path, content: &[u8]) -> io::Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(real)
    {
        Ok(mut file) => {
            file.write_all(content)?;
            file.flush()
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let temp = temp_sibling(real);
            let outcome = fs::write(&temp, content).and_then(|()| fs::rename(&temp, real));
            if outcome.is_err() {
                let _ = fs::remove_file(&temp);
            }
            outcome
        }
        Err(err) => Err(err),
    }
}

fn temp_sibling(real: &Path) -> PathBuf {
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let nonce = hasher.finish();
    let parent = real.parent().unwrap_or_else(|| Path::new("/"));
    parent.join(format!(".{name}.{nonce:016x}.tmp"))
}

/// `\r\n` → `\n`, as the reference server does before matching and diffing.
pub fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn leading_whitespace(line: &str) -> &str {
    let end = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    &line[..end]
}

/// One `edit_file` replacement.
#[derive(Debug, Clone)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// Applies edits sequentially — exact substring first, then the reference
/// server's whitespace-tolerant line-window match that re-indents the
/// replacement to the matched line — and returns the modified text. Any
/// unmatched edit fails the whole call; nothing is written by this function.
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<String, FsError> {
    let mut modified = normalize_line_endings(content);
    for edit in edits {
        let old = normalize_line_endings(&edit.old_text);
        let new = normalize_line_endings(&edit.new_text);
        if old.is_empty() {
            return Err(FsError("oldText must not be empty".into()));
        }
        if let Some(position) = modified.find(&old) {
            modified.replace_range(position..position + old.len(), &new);
            continue;
        }

        let old_lines: Vec<&str> = old.split('\n').collect();
        let mut content_lines: Vec<String> = modified.split('\n').map(str::to_owned).collect();
        let mut matched = false;
        if old_lines.len() <= content_lines.len() {
            for start in 0..=(content_lines.len() - old_lines.len()) {
                let window_matches = old_lines.iter().enumerate().all(|(offset, old_line)| {
                    old_line.trim() == content_lines[start + offset].trim()
                });
                if !window_matches {
                    continue;
                }
                let original_indent = leading_whitespace(&content_lines[start]).to_owned();
                let new_lines: Vec<String> = new
                    .split('\n')
                    .enumerate()
                    .map(|(index, line)| {
                        if index == 0 {
                            return format!("{original_indent}{}", line.trim_start());
                        }
                        let old_indent = old_lines
                            .get(index)
                            .map(|l| leading_whitespace(l))
                            .unwrap_or("");
                        let new_indent = leading_whitespace(line);
                        if !old_indent.is_empty() && !new_indent.is_empty() {
                            let relative = new_indent.len().saturating_sub(old_indent.len());
                            format!(
                                "{original_indent}{}{}",
                                " ".repeat(relative),
                                line.trim_start()
                            )
                        } else {
                            line.to_owned()
                        }
                    })
                    .collect();
                content_lines.splice(start..start + old_lines.len(), new_lines);
                modified = content_lines.join("\n");
                matched = true;
                break;
            }
        }
        if !matched {
            return Err(FsError(format!(
                "Could not find exact match for edit:\n{}",
                edit.old_text
            )));
        }
    }
    Ok(modified)
}

/// A git-style unified diff (3 context lines) in a backtick fence that is
/// always longer than any backtick run inside the diff.
pub fn fenced_diff(original: &str, modified: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(original, modified);
    let body = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    let mut ticks = 3;
    while ticks < 64 && body.contains(&"`".repeat(ticks)) {
        ticks += 1;
    }
    let fence = "`".repeat(ticks);
    format!("{fence}diff\n{body}{fence}\n\n")
}

/// The MIME type `read_media_file` reports for a file, by extension
/// (Node's `path.extname` rules: a leading dot alone is not an extension).
pub fn mime_for(path: &str) -> &'static str {
    let name = file_name_of(path);
    let ext = match name.rfind('.') {
        Some(0) | None => return "application/octet-stream",
        Some(index) => name[index + 1..].to_ascii_lowercase(),
    };
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

/// ISO-8601 UTC timestamp, or "unknown" when the host does not expose it.
pub fn timestamp(value: io::Result<SystemTime>) -> String {
    match value.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(duration) => format_utc(duration.as_secs()),
        None => "unknown".to_owned(),
    }
}

fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil inverse (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// One directory entry with the type its target has (symlinks are followed
/// for the type; an unreadable or dangling link counts as a file).
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// Present only when the sandbox refused to follow a symlink.
    pub escapes: bool,
}

/// Lists a directory sorted by name.
pub fn list_entries(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let file_type = entry.file_type()?;
        let (is_dir, escapes) = if file_type.is_symlink() {
            match fs::metadata(&path) {
                Ok(meta) => (meta.is_dir(), false),
                Err(err) => (false, err.kind() == io::ErrorKind::PermissionDenied),
            }
        } else {
            (file_type.is_dir(), false)
        };
        entries.push(Entry {
            name,
            path,
            is_dir,
            is_symlink: file_type.is_symlink(),
            escapes,
        });
    }
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Compiles glob patterns minimatch-style: `*` never crosses `/`, backslash
/// escapes, dot-files match.
///
/// Search patterns: a bare name (no `/`, no glob metacharacters) is also
/// tried as `**/<name>` so `README.md` finds the file anywhere. Exclude
/// patterns without a `*` are also tried as `**/<pattern>` and
/// `**/<pattern>/**`, exactly as the reference server's `directory_tree`
/// does, so `node_modules` prunes every such directory.
pub fn compile_globs(patterns: &[String], excludes: bool) -> Result<GlobSet, FsError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        builder.add(compile_glob(pattern)?);
        if pattern.contains('/') {
            continue;
        }
        if excludes {
            if !pattern.contains('*') {
                builder.add(compile_glob(&format!("**/{pattern}"))?);
                builder.add(compile_glob(&format!("**/{pattern}/**"))?);
            }
        } else if !pattern.contains(['*', '?', '[', '{']) {
            builder.add(compile_glob(&format!("**/{pattern}"))?);
        }
    }
    builder
        .build()
        .map_err(|err| FsError(format!("Invalid glob pattern set: {err}")))
}

fn compile_glob(pattern: &str) -> Result<Glob, FsError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|err| FsError(format!("Invalid glob pattern '{pattern}': {err}")))
}

/// A `directory_tree` node.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub is_dir: bool,
    pub children: Option<Vec<TreeNode>>,
}

impl TreeNode {
    pub fn to_json(&self) -> serde_json::Value {
        let mut node = serde_json::json!({
            "name": self.name,
            "type": if self.is_dir { "directory" } else { "file" },
        });
        if let Some(children) = &self.children {
            node["children"] =
                serde_json::Value::Array(children.iter().map(TreeNode::to_json).collect());
        }
        node
    }
}

/// Limits and bookkeeping shared by the recursive walkers.
pub struct Walk<'a> {
    pub excludes: &'a GlobSet,
    pub max_depth: usize,
    pub max_results: usize,
    pub count: usize,
    pub truncated: bool,
    visited: HashSet<PathBuf>,
}

impl<'a> Walk<'a> {
    pub fn new(cfg: &'a Config, excludes: &'a GlobSet, max_depth: usize) -> Self {
        Self {
            excludes,
            max_depth,
            max_results: cfg.max_results,
            count: 0,
            truncated: false,
            visited: HashSet::new(),
        }
    }

    /// Marks a directory as visited by canonical path; `false` on a cycle
    /// (or when the runtime refuses to resolve it).
    fn enter(&mut self, dir: &Path) -> bool {
        match fs::canonicalize(dir) {
            Ok(real) => self.visited.insert(real),
            Err(_) => false,
        }
    }

    fn excluded(&self, relative: &str) -> bool {
        !relative.is_empty() && self.excludes.is_match(relative)
    }

    /// Builds the reference server's JSON tree. `relative` is the path of
    /// `dir` relative to the search root ("" for the root).
    pub fn tree(&mut self, dir: &Path, relative: &str, depth: usize) -> io::Result<Vec<TreeNode>> {
        let mut nodes = Vec::new();
        if !self.enter(dir) {
            return Ok(nodes);
        }
        for entry in list_entries(dir)? {
            if self.truncated {
                break;
            }
            let entry_relative = join_relative(relative, &entry.name);
            if self.excluded(&entry_relative) {
                continue;
            }
            if self.count >= self.max_results {
                self.truncated = true;
                nodes.push(TreeNode {
                    name: "…".to_owned(),
                    is_dir: false,
                    children: None,
                });
                break;
            }
            self.count += 1;
            let children = if entry.is_dir {
                if depth < self.max_depth {
                    Some(self.tree(&entry.path, &entry_relative, depth + 1)?)
                } else {
                    Some(Vec::new())
                }
            } else {
                None
            };
            nodes.push(TreeNode {
                name: entry.name,
                is_dir: entry.is_dir,
                children,
            });
        }
        Ok(nodes)
    }

    /// The reference server's `searchFilesWithValidation`: every entry whose
    /// root-relative path matches `pattern`, as absolute guest paths.
    pub fn search(
        &mut self,
        dir: &Path,
        relative: &str,
        depth: usize,
        pattern: &GlobSet,
        results: &mut Vec<String>,
    ) -> io::Result<()> {
        if !self.enter(dir) {
            return Ok(());
        }
        for entry in list_entries(dir)? {
            if self.truncated {
                break;
            }
            // Entries the sandbox refuses are skipped, as the reference
            // server skips entries that fail validation.
            if entry.escapes {
                continue;
            }
            let entry_relative = join_relative(relative, &entry.name);
            if self.excluded(&entry_relative) {
                continue;
            }
            if pattern.is_match(&entry_relative) {
                if results.len() >= self.max_results {
                    self.truncated = true;
                    break;
                }
                results.push(entry.path.to_string_lossy().into_owned());
            }
            self.count += 1;
            if entry.is_dir && depth < self.max_depth {
                self.search(&entry.path, &entry_relative, depth + 1, pattern, results)?;
            }
        }
        Ok(())
    }
}

fn join_relative(relative: &str, name: &str) -> String {
    if relative.is_empty() {
        name.to_owned()
    } else {
        format!("{relative}/{name}")
    }
}

/// Moves `source` to `destination`, refusing to overwrite (a symlink at the
/// destination counts), with a copy-then-delete fallback when the two live on
/// different host filesystems (EXDEV).
pub fn move_path(
    source: &Path,
    destination: &Path,
    destination_label: &str,
) -> Result<(), FsError> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(FsError(format!(
            "Destination already exists: {destination_label}"
        )));
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_recursive(source, destination)
                .and_then(|()| {
                    if fs::symlink_metadata(source)?.is_dir() {
                        fs::remove_dir_all(source)
                    } else {
                        fs::remove_file(source)
                    }
                })
                .map_err(|err| {
                    FsError(io_message(
                        "move (copy fallback)",
                        &source.to_string_lossy(),
                        &err,
                    ))
                })
        }
        Err(err) => Err(FsError(io_message("move", &source.to_string_lossy(), &err))),
    }
}

fn copy_recursive(source: &Path, destination: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &destination.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(source, destination).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_paths() {
        assert_eq!(normalize_absolute("/data//a/./b/../c/"), "/data/a/c");
        assert_eq!(normalize_absolute("/data/../../etc"), "/etc");
        assert_eq!(normalize_absolute("/"), "/");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1_572_864), "1.50 MB");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
