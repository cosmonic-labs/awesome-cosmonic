//! `jco:node/fs`: the host filesystem, with Node's descriptors, flags and error shapes.
//!
//! Relative paths resolve against the guest's own working directory (see
//! [`crate::state::VirtualProcess`]), and descriptors are numbers in a per-instance table rather
//! than the host's, so one guest can neither see nor disturb another's.

use std::{
    ffi::{CString, OsStr, OsString},
    fs::{self, File, Metadata},
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    os::{
        fd::{AsRawFd as _, FromRawFd as _},
        unix::{
            ffi::{OsStrExt as _, OsStringExt as _},
            fs::{FileExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
        },
    },
    path::{Path as StdPath, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::fs::{
        CopyOptions, DirectoryEntry, Dirent, Errno, Error, FileType, GlobEntry, GlobExclude, GlobOptions, Host,
        MkdirOptions, Mode, Numeric, OpenMode, Path, PathOrDescriptor, ReadFileOptions, ReadResult, ReaddirOptions,
        ReadvResult, RemoveOptions, StatOptions, StatfsResult, Stats, WriteFileOptions,
    },
    errno::{self, UvError},
    plugin::NodeView,
    state::{Capability, NodeInstance, OpenFile, lock},
};

type FsResult<T> = wasmtime::Result<Result<T, Error>>;

fn denied() -> Error {
    Error {
        name: "Error".into(),
        message: "node:fs requires an explicitly configured filesystem host provider".into(),
        code: Some("ERR_JCO_FS_ADAPTER_REQUIRED".into()),
        errno: None,
        syscall: None,
        path: None,
        dest: None,
    }
}

fn uv_error(error: UvError, syscall: &str, path: Option<&str>, dest: Option<&str>) -> Error {
    Error {
        name: "Error".into(),
        message: errno::syscall_message(error, syscall, path, dest),
        code: Some(error.code.into()),
        errno: Some(Errno::Number(error.node_errno())),
        syscall: Some(syscall.into()),
        path: path.map(str::to_owned),
        dest: dest.map(str::to_owned),
    }
}

fn io_error(error: &io::Error, syscall: &str, path: Option<&str>, dest: Option<&str>) -> Error {
    uv_error(errno::from_io(error), syscall, path, dest)
}

fn bad_descriptor(syscall: &str) -> Error {
    uv_error(errno::from_errno(libc::EBADF), syscall, None, None)
}

/// A Node `TypeError`/`RangeError` raised by argument validation rather than by the OS.
fn coded(name: &str, code: &str, message: String) -> Error {
    Error { name: name.into(), message, code: Some(code.into()), errno: None, syscall: None, path: None, dest: None }
}

/// Node's `SystemError` for `fs.cp` and `fs.rm`, which describe failures in their own words.
fn system_error(code: &str, message: &str, syscall: &str, uv: UvError, path: &str) -> Error {
    Error {
        name: "SystemError".into(),
        message: format!("{message}: {syscall} returned {} ({}) {path}", uv.code, uv.description_for(syscall)),
        code: Some(code.into()),
        errno: Some(Errno::Number(i64::from(uv.errno))),
        syscall: Some(syscall.into()),
        path: Some(path.into()),
        dest: None,
    }
}

impl UvError {
    /// `fs.cp` and `fs.rm` word a few conditions themselves instead of using libuv's text.
    fn description_for(&self, syscall: &str) -> &'static str {
        match (syscall, self.code) {
            ("rm", "EISDIR") => "is a directory",
            ("cp", "EISDIR") => "src is a directory",
            ("cp", "EEXIST") => "target already exists",
            ("cp", "EINVAL") => "src and dest cannot be the same",
            _ => self.description,
        }
    }
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let digits = text.get(index + 1..index + 3).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(digits, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// A guest path: how it was written (for error messages) and where it is on the host.
struct Resolved {
    shown: String,
    host: PathBuf,
}

fn resolve(node: &NodeInstance, path: &Path) -> Resolved {
    let raw: OsString = match path {
        Path::Text(text) => OsString::from(text),
        Path::Bytes(bytes) => OsString::from_vec(bytes.clone()),
        Path::FileUrl(url) => {
            let rest = url.strip_prefix("file://").unwrap_or(url);
            // A `file:` URL may carry a host (`file://localhost/x`); the path starts at the slash.
            let rest = rest.find('/').map_or(rest, |slash| &rest[slash..]);
            OsString::from_vec(percent_decode(rest))
        }
    };
    let shown = raw.to_string_lossy().into_owned();
    let given = StdPath::new(&raw);
    let host = if given.is_absolute() { given.to_path_buf() } else { node.process().cwd.join(given) };
    Resolved { shown, host }
}

fn c_path(path: &StdPath) -> Result<CString, io::Error> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn parse_mode(mode: &Mode) -> Result<u32, Error> {
    match mode {
        Mode::Number(number) => Ok(*number),
        Mode::Symbolic(text) => u32::from_str_radix(text.trim_start_matches("0o"), 8).map_err(|_| {
            coded(
                "TypeError",
                "ERR_INVALID_ARG_VALUE",
                format!("The argument 'mode' must be a 32-bit unsigned integer or an octal string. Received '{text}'"),
            )
        }),
    }
}

/// `fs.open` flag strings, as `lib/internal/fs/utils.js` `stringToFlags` maps them.
pub(crate) fn parse_flags(flags: &str) -> Option<i32> {
    use libc::{O_APPEND, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_SYNC, O_TRUNC, O_WRONLY};
    Some(match flags {
        "r" => O_RDONLY,
        "rs" | "sr" => O_RDONLY | O_SYNC,
        "r+" => O_RDWR,
        "rs+" | "sr+" => O_RDWR | O_SYNC,
        "w" => O_TRUNC | O_CREAT | O_WRONLY,
        "wx" | "xw" => O_TRUNC | O_CREAT | O_WRONLY | O_EXCL,
        "w+" => O_TRUNC | O_CREAT | O_RDWR,
        "wx+" | "xw+" => O_TRUNC | O_CREAT | O_RDWR | O_EXCL,
        "a" => O_APPEND | O_CREAT | O_WRONLY,
        "ax" | "xa" => O_APPEND | O_CREAT | O_WRONLY | O_EXCL,
        "as" | "sa" => O_APPEND | O_CREAT | O_WRONLY | O_SYNC,
        "a+" => O_APPEND | O_CREAT | O_RDWR,
        "ax+" | "xa+" => O_APPEND | O_CREAT | O_RDWR | O_EXCL,
        "as+" | "sa+" => O_APPEND | O_CREAT | O_RDWR | O_SYNC,
        _ => return None,
    })
}

fn flags_of(mode: &OpenMode) -> Result<i32, Error> {
    match mode {
        OpenMode::Number(number) => Ok(*number),
        OpenMode::Symbolic(text) => parse_flags(text).ok_or_else(|| {
            coded("TypeError", "ERR_INVALID_ARG_VALUE", format!("The argument 'flags' is invalid. Received '{text}'"))
        }),
    }
}

fn open_host(path: &StdPath, flags: i32, mode: u32) -> io::Result<File> {
    let path = c_path(path)?;
    // SAFETY: `path` is a valid NUL-terminated string; `open` reads nothing else.
    let descriptor = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, mode as libc::c_uint) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open` just returned this descriptor and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn file_type(metadata: &Metadata) -> FileType {
    let kind = metadata.file_type();
    if kind.is_file() {
        FileType::File
    } else if kind.is_dir() {
        FileType::Directory
    } else if kind.is_symlink() {
        FileType::Symlink
    } else if kind.is_block_device() {
        FileType::Block
    } else if kind.is_char_device() {
        FileType::Character
    } else if kind.is_fifo() {
        FileType::Fifo
    } else if kind.is_socket() {
        FileType::Socket
    } else {
        FileType::Unknown
    }
}

fn stats(metadata: &Metadata, bigint: bool) -> Stats {
    let integer = |value: i128| if bigint { Numeric::Bigint(value as i64) } else { Numeric::Number(value as f64) };
    let nanos = |seconds: i64, nanoseconds: i64| i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds);
    let time = |total: i128| {
        if bigint { Numeric::Bigint((total / 1_000_000) as i64) } else { Numeric::Number(total as f64 / 1e6) }
    };
    let atime = nanos(metadata.atime(), metadata.atime_nsec());
    let mtime = nanos(metadata.mtime(), metadata.mtime_nsec());
    let ctime = nanos(metadata.ctime(), metadata.ctime_nsec());
    let birthtime = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
        .map_or(ctime, |since| since.as_nanos() as i128);
    let exact = |total: i128| bigint.then_some(total as i64);
    Stats {
        dev: integer(i128::from(metadata.dev())),
        ino: integer(i128::from(metadata.ino())),
        mode: integer(i128::from(metadata.mode())),
        nlink: integer(i128::from(metadata.nlink())),
        uid: integer(i128::from(metadata.uid())),
        gid: integer(i128::from(metadata.gid())),
        rdev: integer(i128::from(metadata.rdev())),
        size: integer(i128::from(metadata.size())),
        blksize: integer(i128::from(metadata.blksize())),
        blocks: integer(i128::from(metadata.blocks())),
        atime_ms: time(atime),
        mtime_ms: time(mtime),
        ctime_ms: time(ctime),
        birthtime_ms: time(birthtime),
        atime_ns: exact(atime),
        mtime_ns: exact(mtime),
        ctime_ns: exact(ctime),
        birthtime_ns: exact(birthtime),
        file_type: file_type(metadata),
    }
}

fn timespec(seconds: f64) -> libc::timespec {
    let whole = seconds.floor();
    libc::timespec { tv_sec: whole as libc::time_t, tv_nsec: ((seconds - whole) * 1e9) as libc::c_long }
}

fn set_times(path: &StdPath, atime: f64, mtime: f64, follow: bool) -> io::Result<()> {
    let path = c_path(path)?;
    let times = [timespec(atime), timespec(mtime)];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: `path` is NUL-terminated and `times` is the two-element array `utimensat` reads.
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn chown(path: &StdPath, uid: u32, gid: u32, follow: bool) -> io::Result<()> {
    let path = c_path(path)?;
    // SAFETY: `path` is a valid NUL-terminated string.
    let status = unsafe {
        if follow { libc::chown(path.as_ptr(), uid, gid) } else { libc::lchown(path.as_ptr(), uid, gid) }
    };
    if status != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn dirent(entry_path: &StdPath, parent: &str, metadata: Option<&Metadata>) -> Dirent {
    Dirent {
        name: entry_path.file_name().unwrap_or(OsStr::new("")).to_string_lossy().into_owned(),
        parent_path: parent.to_owned(),
        file_type: metadata.map_or(FileType::Unknown, file_type),
    }
}

/// List a directory in the order the OS returns it, as Node does.
fn read_names(directory: &StdPath) -> io::Result<Vec<OsString>> {
    fs::read_dir(directory)?.map(|entry| entry.map(|entry| entry.file_name())).collect()
}

fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 6];
    // SAFETY: `bytes` is six writable bytes, which is what is asked for.
    let filled = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if filled != bytes.len() as isize {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (seed >> (index * 8)) as u8;
        }
    }
    bytes.iter().map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char).collect()
}

/// Match one path segment against a glob segment: `*`, `?`, `[a-z]`, `[!a]`.
fn segment_matches(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|skip| segment_matches(rest, &name[skip..])),
        Some(('?', rest)) => !name.is_empty() && segment_matches(rest, &name[1..]),
        Some(('[', rest)) => {
            let Some(close) = rest.iter().position(|character| *character == ']').filter(|close| *close > 0) else {
                return name.first() == Some(&'[') && segment_matches(rest, &name[1..]);
            };
            let Some((first, remaining)) = name.split_first() else { return false };
            let (negated, class) = match rest[..close].split_first() {
                Some((&('!' | '^'), class)) => (true, class),
                _ => (false, &rest[..close]),
            };
            let mut found = false;
            let mut index = 0;
            while index < class.len() {
                if index + 2 < class.len() && class[index + 1] == '-' {
                    found |= (class[index]..=class[index + 2]).contains(first);
                    index += 3;
                } else {
                    found |= class[index] == *first;
                    index += 1;
                }
            }
            found != negated && segment_matches(&rest[close + 1..], remaining)
        }
        Some((literal, rest)) => name.first() == Some(literal) && segment_matches(rest, &name[1..]),
    }
}

/// Expand `{a,b}` alternatives into plain patterns.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.to_owned()] };
    let mut depth = 0;
    let mut close = None;
    for (index, character) in pattern[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + index);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else { return vec![pattern.to_owned()] };
    let mut alternatives = Vec::new();
    let mut depth = 0;
    let mut start = open + 1;
    for (index, character) in pattern[open + 1..close].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                alternatives.push(&pattern[start..open + 1 + index]);
                start = open + 2 + index;
            }
            _ => {}
        }
    }
    alternatives.push(&pattern[start..close]);
    alternatives
        .into_iter()
        .flat_map(|alternative| expand_braces(&format!("{}{alternative}{}", &pattern[..open], &pattern[close + 1..])))
        .collect()
}

fn glob_walk(base: &StdPath, relative: &str, segments: &[&str], out: &mut Vec<String>) {
    let Some((segment, rest)) = segments.split_first() else {
        if !relative.is_empty() {
            out.push(relative.to_owned());
        }
        return;
    };
    let here = if relative.is_empty() { base.to_path_buf() } else { base.join(relative) };
    let join = |name: &str| if relative.is_empty() { name.to_owned() } else { format!("{relative}/{name}") };
    if *segment == "**" {
        // `**` matches zero or more directories.
        glob_walk(base, relative, rest, out);
        for name in read_names(&here).unwrap_or_default() {
            let name = name.to_string_lossy().into_owned();
            if fs::symlink_metadata(here.join(&name)).is_ok_and(|metadata| metadata.is_dir()) {
                glob_walk(base, &join(&name), segments, out);
            }
        }
        return;
    }
    if !segment.contains(['*', '?', '[']) {
        if fs::symlink_metadata(here.join(segment)).is_ok() {
            glob_walk(base, &join(segment), rest, out);
        }
        return;
    }
    let pattern: Vec<char> = segment.chars().collect();
    for name in read_names(&here).unwrap_or_default() {
        let name = name.to_string_lossy().into_owned();
        // A wildcard does not match a leading dot unless the pattern spells one.
        if name.starts_with('.') && !segment.starts_with('.') {
            continue;
        }
        if segment_matches(&pattern, &name.chars().collect::<Vec<_>>()) {
            glob_walk(base, &join(&name), rest, out);
        }
    }
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    fn walk(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| walk(rest, &path[skip..])),
            Some((segment, rest)) => path.split_first().is_some_and(|(name, remaining)| {
                segment_matches(&segment.chars().collect::<Vec<_>>(), &name.chars().collect::<Vec<_>>())
                    && walk(rest, remaining)
            }),
        }
    }
    expand_braces(pattern).iter().any(|pattern| {
        walk(&pattern.split('/').collect::<Vec<_>>(), &path.split('/').collect::<Vec<_>>())
    })
}

fn copy_recursive(source: &StdPath, destination: &StdPath, options: &CopyOptions) -> io::Result<()> {
    let metadata = if options.dereference { fs::metadata(source)? } else { fs::symlink_metadata(source)? };
    if metadata.is_dir() {
        if let Err(error) = fs::create_dir(destination) {
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        for name in read_names(source)? {
            copy_recursive(&source.join(&name), &destination.join(&name), options)?;
        }
    } else if metadata.file_type().is_symlink() {
        let mut target = fs::read_link(source)?;
        if !options.verbatim_symlinks && target.is_relative() {
            target = source.parent().unwrap_or(StdPath::new("/")).join(target);
        }
        if fs::symlink_metadata(destination).is_ok() {
            fs::remove_file(destination)?;
        }
        std::os::unix::fs::symlink(target, destination)?;
        return Ok(());
    } else {
        if fs::symlink_metadata(destination).is_ok() {
            if options.error_on_exist && !options.force {
                return Err(io::Error::from_raw_os_error(libc::EEXIST));
            }
            if !options.force {
                return Ok(());
            }
        }
        fs::copy(source, destination)?;
    }
    if options.preserve_timestamps {
        let seconds = |whole: i64, nanoseconds: i64| whole as f64 + nanoseconds as f64 / 1e9;
        set_times(
            destination,
            seconds(metadata.atime(), metadata.atime_nsec()),
            seconds(metadata.mtime(), metadata.mtime_nsec()),
            true,
        )?;
    }
    Ok(())
}

impl NodeView<'_> {
    fn fs_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Fs) { Ok(()) } else { Err(denied()) }
    }

    /// Run `operation` on the open file behind `descriptor`.
    fn with_file<T>(
        &self,
        descriptor: u32,
        syscall: &str,
        operation: impl FnOnce(&mut OpenFile) -> io::Result<T>,
    ) -> Result<T, Error> {
        let mut descriptors = self.node.descriptors();
        let file = descriptors.get_mut(descriptor).ok_or_else(|| bad_descriptor(syscall))?;
        operation(file).map_err(|error| io_error(&error, syscall, None, None))
    }

    fn write_target(
        &self,
        file: &PathOrDescriptor,
        data: &[u8],
        options: &WriteFileOptions,
        default_flag: &str,
    ) -> Result<(), Error> {
        match file {
            PathOrDescriptor::Descriptor(descriptor) => self.with_file(*descriptor, "write", |open| {
                open.file.write_all(data)?;
                if options.flush {
                    open.file.sync_all()?;
                }
                Ok(())
            }),
            PathOrDescriptor::Path(path) => {
                let path = resolve(&self.node, path);
                let flag = options.flag.as_deref().unwrap_or(default_flag);
                let flags = flags_of(&OpenMode::Symbolic(flag.to_owned()))?;
                let mode = options.mode.as_ref().map(parse_mode).transpose()?.unwrap_or(0o666);
                let mut file = open_host(&path.host, flags, mode)
                    .map_err(|error| io_error(&error, "open", Some(&path.shown), None))?;
                file.write_all(data).map_err(|error| io_error(&error, "write", None, None))?;
                if options.flush {
                    file.sync_all().map_err(|error| io_error(&error, "fsync", None, None))?;
                }
                Ok(())
            }
        }
    }
}

macro_rules! granted {
    ($view:expr, $name:literal) => {
        if let Err(error) = $view.fs_granted() {
            return Ok(Err(error));
        }
        // Every call here blocks until it is done, which is the category Node files them under.
        lock(&$view.node.tracing).record($view.node.started, "node.fs.sync", $name);
    };
}

/// Run a fallible body and hand its `Result` to the guest.
macro_rules! attempt {
    ($body:block) => {
        Ok((|| -> Result<_, Error> { $body })())
    };
}

impl Host for NodeView<'_> {
    async fn access(&mut self, path: Path, mode: i32) -> FsResult<()> {
        granted!(self, "access");
        attempt!({
            let path = resolve(&self.node, &path);
            let name = c_path(&path.host).map_err(|error| io_error(&error, "access", Some(&path.shown), None))?;
            // SAFETY: `name` is a valid NUL-terminated string.
            if unsafe { libc::access(name.as_ptr(), mode) } != 0 {
                return Err(io_error(&io::Error::last_os_error(), "access", Some(&path.shown), None));
            }
            Ok(())
        })
    }

    async fn append_file(&mut self, file: PathOrDescriptor, data: Vec<u8>, options: WriteFileOptions) -> FsResult<()> {
        granted!(self, "append_file");
        Ok(self.write_target(&file, &data, &options, "a"))
    }

    async fn chmod(&mut self, path: Path, mode: Mode) -> FsResult<()> {
        granted!(self, "chmod");
        attempt!({
            let path = resolve(&self.node, &path);
            let mode = parse_mode(&mode)?;
            fs::set_permissions(&path.host, fs::Permissions::from_mode(mode))
                .map_err(|error| io_error(&error, "chmod", Some(&path.shown), None))
        })
    }

    async fn chown(&mut self, path: Path, uid: u32, gid: u32) -> FsResult<()> {
        granted!(self, "chown");
        attempt!({
            let path = resolve(&self.node, &path);
            chown(&path.host, uid, gid, true).map_err(|error| io_error(&error, "chown", Some(&path.shown), None))
        })
    }

    async fn close(&mut self, descriptor: u32) -> FsResult<()> {
        granted!(self, "close");
        Ok(self.node.descriptors().remove(descriptor).map(drop).ok_or_else(|| bad_descriptor("close")))
    }

    async fn copy_file(&mut self, source: Path, destination: Path, mode: i32) -> FsResult<()> {
        granted!(self, "copy_file");
        attempt!({
            const COPYFILE_EXCL: i32 = 1;
            const COPYFILE_FICLONE_FORCE: i32 = 4;
            let source = resolve(&self.node, &source);
            let destination = resolve(&self.node, &destination);
            let fail = |error: &io::Error| io_error(error, "copyfile", Some(&source.shown), Some(&destination.shown));
            if mode & COPYFILE_FICLONE_FORCE != 0 {
                // A forced reflink has no portable implementation here.
                return Err(fail(&io::Error::from_raw_os_error(libc::ENOSYS)));
            }
            if mode & COPYFILE_EXCL != 0 && fs::symlink_metadata(&destination.host).is_ok() {
                return Err(fail(&io::Error::from_raw_os_error(libc::EEXIST)));
            }
            fs::copy(&source.host, &destination.host).map(drop).map_err(|error| fail(&error))
        })
    }

    async fn cp(&mut self, source: Path, destination: Path, options: CopyOptions) -> FsResult<()> {
        granted!(self, "cp");
        attempt!({
            let source = resolve(&self.node, &source);
            let destination = resolve(&self.node, &destination);
            let metadata = if options.dereference {
                fs::metadata(&source.host)
            } else {
                fs::symlink_metadata(&source.host)
            }
            .map_err(|error| io_error(&error, "cp", Some(&source.shown), None))?;
            if fs::canonicalize(&source.host).ok() == fs::canonicalize(&destination.host).ok()
                && fs::symlink_metadata(&destination.host).is_ok()
            {
                return Err(system_error(
                    "ERR_FS_CP_EINVAL",
                    "Invalid src or dest",
                    "cp",
                    errno::from_errno(libc::EINVAL),
                    &destination.shown,
                ));
            }
            if metadata.is_dir() && !options.recursive {
                return Err(system_error(
                    "ERR_FS_EISDIR",
                    "Recursive option not enabled, cannot copy a directory",
                    "cp",
                    errno::from_errno(libc::EISDIR),
                    &source.shown,
                ));
            }
            if !metadata.is_dir()
                && options.error_on_exist
                && !options.force
                && fs::symlink_metadata(&destination.host).is_ok()
            {
                return Err(system_error(
                    "ERR_FS_CP_EEXIST",
                    "Target already exists",
                    "cp",
                    errno::from_errno(libc::EEXIST),
                    &destination.shown,
                ));
            }
            if let Some(parent) = destination.host.parent() {
                fs::create_dir_all(parent).map_err(|error| io_error(&error, "mkdir", Some(&destination.shown), None))?;
            }
            copy_recursive(&source.host, &destination.host, &options)
                .map_err(|error| io_error(&error, "cp", Some(&source.shown), Some(&destination.shown)))
        })
    }

    async fn exists(&mut self, path: Path) -> FsResult<bool> {
        granted!(self, "exists");
        Ok(Ok(fs::metadata(resolve(&self.node, &path).host).is_ok()))
    }

    async fn fchmod(&mut self, descriptor: u32, mode: Mode) -> FsResult<()> {
        granted!(self, "fchmod");
        attempt!({
            let mode = parse_mode(&mode)?;
            self.with_file(descriptor, "fchmod", |open| open.file.set_permissions(fs::Permissions::from_mode(mode)))
        })
    }

    async fn fchown(&mut self, descriptor: u32, uid: u32, gid: u32) -> FsResult<()> {
        granted!(self, "fchown");
        Ok(self.with_file(descriptor, "fchown", |open| {
            // SAFETY: the descriptor is open for as long as `open` is borrowed.
            if unsafe { libc::fchown(open.file.as_raw_fd(), uid, gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }))
    }

    async fn fdatasync(&mut self, descriptor: u32) -> FsResult<()> {
        granted!(self, "fdatasync");
        Ok(self.with_file(descriptor, "fdatasync", |open| open.file.sync_data()))
    }

    async fn fstat(&mut self, descriptor: u32, options: StatOptions) -> FsResult<Stats> {
        granted!(self, "fstat");
        Ok(self.with_file(descriptor, "fstat", |open| open.file.metadata()).map(|meta| stats(&meta, options.bigint)))
    }

    async fn fsync(&mut self, descriptor: u32) -> FsResult<()> {
        granted!(self, "fsync");
        Ok(self.with_file(descriptor, "fsync", |open| open.file.sync_all()))
    }

    async fn ftruncate(&mut self, descriptor: u32, length: u32) -> FsResult<()> {
        granted!(self, "ftruncate");
        Ok(self.with_file(descriptor, "ftruncate", |open| open.file.set_len(u64::from(length))))
    }

    async fn futimes(&mut self, descriptor: u32, atime: f64, mtime: f64) -> FsResult<()> {
        granted!(self, "futimes");
        Ok(self.with_file(descriptor, "futime", |open| {
            let times = [timespec(atime), timespec(mtime)];
            // SAFETY: the descriptor is open, and `times` is the two-element array read.
            if unsafe { libc::futimens(open.file.as_raw_fd(), times.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }))
    }

    async fn glob(&mut self, patterns: Vec<String>, options: GlobOptions) -> FsResult<Vec<GlobEntry>> {
        granted!(self, "glob");
        attempt!({
            let base = match &options.cwd {
                Some(cwd) => resolve(&self.node, cwd).host,
                None => self.node.process().cwd.clone(),
            };
            let excluded: Vec<String> = match &options.exclude {
                None => Vec::new(),
                Some(GlobExclude::Pattern(pattern)) => vec![pattern.clone()],
                Some(GlobExclude::Patterns(patterns)) => patterns.clone(),
            };
            let mut found = Vec::new();
            for pattern in patterns.iter().flat_map(|pattern| expand_braces(pattern)) {
                let (root, pattern) = match pattern.strip_prefix('/') {
                    Some(rest) => (PathBuf::from("/"), rest.to_owned()),
                    None => (base.clone(), pattern.clone()),
                };
                let segments: Vec<&str> = pattern.split('/').filter(|segment| !segment.is_empty() && *segment != ".").collect();
                let mut matches = Vec::new();
                glob_walk(&root, "", &segments, &mut matches);
                for path in matches {
                    let shown = if root == StdPath::new("/") && base != root { format!("/{path}") } else { path };
                    if !found.contains(&shown) {
                        found.push(shown);
                    }
                }
            }
            found.retain(|path| !excluded.iter().any(|pattern| glob_matches(pattern, path)));
            Ok(found
                .into_iter()
                .map(|path| {
                    if !options.with_file_types {
                        return GlobEntry::Path(path);
                    }
                    let host = base.join(&path);
                    let parent = host.parent().unwrap_or(&base).to_string_lossy().into_owned();
                    GlobEntry::Dirent(dirent(&host, &parent, fs::symlink_metadata(&host).ok().as_ref()))
                })
                .collect())
        })
    }

    async fn lchown(&mut self, path: Path, uid: u32, gid: u32) -> FsResult<()> {
        granted!(self, "lchown");
        attempt!({
            let path = resolve(&self.node, &path);
            chown(&path.host, uid, gid, false).map_err(|error| io_error(&error, "lchown", Some(&path.shown), None))
        })
    }

    async fn link(&mut self, existing_path: Path, new_path: Path) -> FsResult<()> {
        granted!(self, "link");
        attempt!({
            let existing = resolve(&self.node, &existing_path);
            let new = resolve(&self.node, &new_path);
            fs::hard_link(&existing.host, &new.host)
                .map_err(|error| io_error(&error, "link", Some(&existing.shown), Some(&new.shown)))
        })
    }

    async fn lstat(&mut self, path: Path, options: StatOptions) -> FsResult<Option<Stats>> {
        granted!(self, "lstat");
        attempt!({
            let path = resolve(&self.node, &path);
            match fs::symlink_metadata(&path.host) {
                Ok(metadata) => Ok(Some(stats(&metadata, options.bigint))),
                Err(error) if error.kind() == io::ErrorKind::NotFound && !options.throw_if_no_entry => Ok(None),
                Err(error) => Err(io_error(&error, "lstat", Some(&path.shown), None)),
            }
        })
    }

    async fn lutimes(&mut self, path: Path, atime: f64, mtime: f64) -> FsResult<()> {
        granted!(self, "lutimes");
        attempt!({
            let path = resolve(&self.node, &path);
            set_times(&path.host, atime, mtime, false)
                .map_err(|error| io_error(&error, "lutime", Some(&path.shown), None))
        })
    }

    async fn mkdir(&mut self, path: Path, options: MkdirOptions) -> FsResult<Option<String>> {
        granted!(self, "mkdir");
        attempt!({
            let path = resolve(&self.node, &path);
            let mode = options.mode.as_ref().map(parse_mode).transpose()?.unwrap_or(0o777);
            let create = |target: &StdPath| -> io::Result<()> {
                let name = c_path(target)?;
                // SAFETY: `name` is a valid NUL-terminated string.
                if unsafe { libc::mkdir(name.as_ptr(), mode as libc::mode_t) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            };
            let fail = |error: &io::Error| io_error(error, "mkdir", Some(&path.shown), None);
            if !options.recursive {
                return create(&path.host).map(|()| None).map_err(|error| fail(&error));
            }
            // Recursive `mkdir` reports the first directory it had to create, or nothing.
            let mut missing = Vec::new();
            let mut cursor = Some(path.host.as_path());
            while let Some(candidate) = cursor {
                match fs::metadata(candidate) {
                    Ok(metadata) if metadata.is_dir() => break,
                    Ok(_) => return Err(fail(&io::Error::from_raw_os_error(libc::EEXIST))),
                    Err(_) => missing.push(candidate.to_path_buf()),
                }
                cursor = candidate.parent();
            }
            let first = missing.last().cloned();
            for directory in missing.iter().rev() {
                if let Err(error) = create(directory) {
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(fail(&error));
                    }
                }
            }
            Ok(first.map(|first| first.to_string_lossy().into_owned()))
        })
    }

    async fn mkdtemp(&mut self, prefix: String) -> FsResult<String> {
        granted!(self, "mkdtemp");
        attempt!({
            let shown = format!("{prefix}XXXXXX");
            for _ in 0..64 {
                let candidate = format!("{prefix}{}", random_suffix());
                let host = self.node.resolve(&candidate);
                match fs::create_dir(&host) {
                    Ok(()) => {
                        let _ = fs::set_permissions(&host, fs::Permissions::from_mode(0o700));
                        return Ok(candidate);
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(io_error(&error, "mkdtemp", Some(&shown), None)),
                }
            }
            Err(uv_error(errno::from_errno(libc::EEXIST), "mkdtemp", Some(&shown), None))
        })
    }

    async fn open(&mut self, path: Path, flags: OpenMode, mode: Mode) -> FsResult<u32> {
        granted!(self, "open");
        attempt!({
            let path = resolve(&self.node, &path);
            let flags = flags_of(&flags)?;
            let mode = parse_mode(&mode)?;
            let file =
                open_host(&path.host, flags, mode).map_err(|error| io_error(&error, "open", Some(&path.shown), None))?;
            Ok(self.node.descriptors().insert(OpenFile {
                file,
                path: path.shown,
                append: flags & libc::O_APPEND != 0,
            }))
        })
    }

    async fn read_file(&mut self, file: PathOrDescriptor, options: ReadFileOptions) -> FsResult<Vec<u8>> {
        granted!(self, "read_file");
        attempt!({
            match &file {
                PathOrDescriptor::Descriptor(descriptor) => self.with_file(*descriptor, "read", |open| {
                    let mut data = Vec::new();
                    open.file.read_to_end(&mut data)?;
                    Ok(data)
                }),
                PathOrDescriptor::Path(path) => {
                    let path = resolve(&self.node, path);
                    let flags = flags_of(&OpenMode::Symbolic(options.flag.clone().unwrap_or_else(|| "r".into())))?;
                    let mut file = open_host(&path.host, flags, 0o666)
                        .map_err(|error| io_error(&error, "open", Some(&path.shown), None))?;
                    let mut data = Vec::new();
                    // Reading a directory fails here, in `read`, with EISDIR and no path.
                    file.read_to_end(&mut data).map_err(|error| io_error(&error, "read", None, None))?;
                    Ok(data)
                }
            }
        })
    }

    async fn readdir(&mut self, path: Path, options: ReaddirOptions) -> FsResult<Vec<DirectoryEntry>> {
        granted!(self, "readdir");
        attempt!({
            let path = resolve(&self.node, &path);
            let mut entries = Vec::new();
            // Breadth-first, as Node's recursive readdir walks.
            let mut pending = std::collections::VecDeque::from([(path.host.clone(), String::new())]);
            let mut first = true;
            while let Some((directory, relative)) = pending.pop_front() {
                let names = match read_names(&directory) {
                    Ok(names) => names,
                    Err(error) if first => return Err(io_error(&error, "scandir", Some(&path.shown), None)),
                    Err(_) => continue,
                };
                first = false;
                for name in names {
                    let name = name.to_string_lossy().into_owned();
                    let host = directory.join(&name);
                    let metadata = fs::symlink_metadata(&host).ok();
                    let joined = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
                    if options.recursive && metadata.as_ref().is_some_and(Metadata::is_dir) {
                        pending.push_back((host.clone(), joined.clone()));
                    }
                    entries.push(if options.with_file_types {
                        let parent = if relative.is_empty() {
                            path.shown.clone()
                        } else {
                            format!("{}/{relative}", path.shown.trim_end_matches('/'))
                        };
                        DirectoryEntry::Dirent(dirent(&host, &parent, metadata.as_ref()))
                    } else {
                        DirectoryEntry::Name(joined)
                    });
                }
            }
            Ok(entries)
        })
    }

    async fn readlink(&mut self, path: Path) -> FsResult<String> {
        granted!(self, "readlink");
        attempt!({
            let path = resolve(&self.node, &path);
            fs::read_link(&path.host)
                .map(|target| target.to_string_lossy().into_owned())
                .map_err(|error| io_error(&error, "readlink", Some(&path.shown), None))
        })
    }

    async fn realpath(&mut self, path: Path) -> FsResult<String> {
        granted!(self, "realpath");
        attempt!({
            let path = resolve(&self.node, &path);
            fs::canonicalize(&path.host)
                .map(|real| real.to_string_lossy().into_owned())
                // Node resolves link by link with `lstat`, and names that call when one is missing.
                .map_err(|error| io_error(&error, "lstat", Some(&path.shown), None))
        })
    }

    async fn rename(&mut self, old_path: Path, new_path: Path) -> FsResult<()> {
        granted!(self, "rename");
        attempt!({
            let old = resolve(&self.node, &old_path);
            let new = resolve(&self.node, &new_path);
            fs::rename(&old.host, &new.host)
                .map_err(|error| io_error(&error, "rename", Some(&old.shown), Some(&new.shown)))
        })
    }

    async fn rm(&mut self, path: Path, options: RemoveOptions) -> FsResult<()> {
        granted!(self, "rm");
        attempt!({
            let path = resolve(&self.node, &path);
            let metadata = match fs::symlink_metadata(&path.host) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound && options.force => return Ok(()),
                Err(error) => return Err(io_error(&error, "rm", Some(&path.shown), None)),
            };
            if !metadata.is_dir() {
                return fs::remove_file(&path.host).map_err(|error| io_error(&error, "rm", Some(&path.shown), None));
            }
            if !options.recursive {
                return Err(system_error(
                    "ERR_FS_EISDIR",
                    "Path is a directory",
                    "rm",
                    errno::from_errno(libc::EISDIR),
                    &path.shown,
                ));
            }
            let mut attempts = 0;
            loop {
                match fs::remove_dir_all(&path.host) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        let retryable = matches!(
                            error.raw_os_error(),
                            Some(libc::EBUSY | libc::EMFILE | libc::ENFILE | libc::ENOTEMPTY | libc::EPERM)
                        );
                        if !retryable || attempts >= options.max_retries {
                            return Err(io_error(&error, "rm", Some(&path.shown), None));
                        }
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_millis(u64::from(options.retry_delay)));
                    }
                }
            }
        })
    }

    async fn rmdir(&mut self, path: Path, _options: RemoveOptions) -> FsResult<()> {
        granted!(self, "rmdir");
        attempt!({
            let path = resolve(&self.node, &path);
            fs::remove_dir(&path.host).map_err(|error| io_error(&error, "rmdir", Some(&path.shown), None))
        })
    }

    async fn stat(&mut self, path: Path, options: StatOptions) -> FsResult<Option<Stats>> {
        granted!(self, "stat");
        attempt!({
            let path = resolve(&self.node, &path);
            match fs::metadata(&path.host) {
                Ok(metadata) => Ok(Some(stats(&metadata, options.bigint))),
                Err(error) if error.kind() == io::ErrorKind::NotFound && !options.throw_if_no_entry => Ok(None),
                Err(error) => Err(io_error(&error, "stat", Some(&path.shown), None)),
            }
        })
    }

    async fn statfs(&mut self, path: Path, bigint: bool) -> FsResult<StatfsResult> {
        granted!(self, "statfs");
        attempt!({
            let path = resolve(&self.node, &path);
            let fail = |error: &io::Error| io_error(error, "statfs", Some(&path.shown), None);
            let name = c_path(&path.host).map_err(|error| fail(&error))?;
            // SAFETY: `statfs` is plain data, valid when zeroed.
            let mut info: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: `name` is NUL-terminated and `info` is a valid, writable `statfs`.
            if unsafe { libc::statfs(name.as_ptr(), &mut info) } != 0 {
                return Err(fail(&io::Error::last_os_error()));
            }
            let numeric = |value: i128| if bigint { Numeric::Bigint(value as i64) } else { Numeric::Number(value as f64) };
            Ok(StatfsResult {
                type_: numeric(info.f_type as i128),
                bsize: numeric(info.f_bsize as i128),
                blocks: numeric(info.f_blocks as i128),
                bfree: numeric(info.f_bfree as i128),
                bavail: numeric(info.f_bavail as i128),
                files: numeric(info.f_files as i128),
                ffree: numeric(info.f_ffree as i128),
            })
        })
    }

    async fn symlink(&mut self, target: Path, path: Path, type_: Option<String>) -> FsResult<()> {
        granted!(self, "symlink");
        attempt!({
            if let Some(kind) = type_.as_deref().filter(|kind| !matches!(*kind, "dir" | "file" | "junction")) {
                return Err(coded("TypeError", "ERR_INVALID_ARG_VALUE", format!("invalid symlink type: {kind}")));
            }
            // The target is stored as written: a relative one is relative to the link, not to
            // the guest's working directory.
            let target_text = match &target {
                Path::Text(text) => OsString::from(text),
                Path::Bytes(bytes) => OsString::from_vec(bytes.clone()),
                Path::FileUrl(_) => resolve(&self.node, &target).host.into_os_string(),
            };
            let path = resolve(&self.node, &path);
            std::os::unix::fs::symlink(&target_text, &path.host).map_err(|error| {
                io_error(&error, "symlink", Some(&target_text.to_string_lossy()), Some(&path.shown))
            })
        })
    }

    async fn truncate(&mut self, path: Path, length: u32) -> FsResult<()> {
        granted!(self, "truncate");
        attempt!({
            let path = resolve(&self.node, &path);
            let file = open_host(&path.host, libc::O_WRONLY, 0)
                .map_err(|error| io_error(&error, "open", Some(&path.shown), None))?;
            file.set_len(u64::from(length)).map_err(|error| io_error(&error, "ftruncate", None, None))
        })
    }

    async fn unlink(&mut self, path: Path) -> FsResult<()> {
        granted!(self, "unlink");
        attempt!({
            let path = resolve(&self.node, &path);
            fs::remove_file(&path.host).map_err(|error| io_error(&error, "unlink", Some(&path.shown), None))
        })
    }

    async fn utimes(&mut self, path: Path, atime: f64, mtime: f64) -> FsResult<()> {
        granted!(self, "utimes");
        attempt!({
            let path = resolve(&self.node, &path);
            set_times(&path.host, atime, mtime, true).map_err(|error| io_error(&error, "utime", Some(&path.shown), None))
        })
    }

    async fn write_file(&mut self, file: PathOrDescriptor, data: Vec<u8>, options: WriteFileOptions) -> FsResult<()> {
        granted!(self, "write_file");
        Ok(self.write_target(&file, &data, &options, "w"))
    }

    async fn read(&mut self, descriptor: u32, length: u32, position: Option<u64>) -> FsResult<ReadResult> {
        granted!(self, "read");
        Ok(self.with_file(descriptor, "read", |open| {
            let mut data = vec![0u8; length as usize];
            let read = match position {
                Some(position) => open.file.read_at(&mut data, position)?,
                None => open.file.read(&mut data)?,
            };
            data.truncate(read);
            Ok(ReadResult { bytes_read: read as u32, data })
        }))
    }

    async fn write(&mut self, descriptor: u32, data: Vec<u8>, position: Option<u64>) -> FsResult<u32> {
        granted!(self, "write");
        Ok(self.with_file(descriptor, "write", |open| {
            let written = match position {
                // An append-mode file ignores the position, as `pwrite` on Linux does.
                Some(position) if !open.append => open.file.write_at(&data, position)?,
                _ => open.file.write(&data)?,
            };
            Ok(written as u32)
        }))
    }

    async fn readv(&mut self, descriptor: u32, lengths: Vec<u32>, position: Option<u64>) -> FsResult<ReadvResult> {
        granted!(self, "readv");
        Ok(self.with_file(descriptor, "read", |open| {
            if let Some(position) = position {
                open.file.seek(SeekFrom::Start(position))?;
            }
            let mut total = 0u32;
            let mut buffers = Vec::with_capacity(lengths.len());
            let mut exhausted = false;
            for length in lengths {
                // Every buffer comes back at its full length, as Node leaves the unread tail
                // of the caller's buffers untouched.
                let mut buffer = vec![0u8; length as usize];
                let mut filled = 0;
                while !exhausted && filled < buffer.len() {
                    match open.file.read(&mut buffer[filled..])? {
                        0 => exhausted = true,
                        read => filled += read,
                    }
                }
                total += filled as u32;
                buffers.push(buffer);
            }
            Ok(ReadvResult { bytes_read: total, buffers })
        }))
    }

    async fn writev(&mut self, descriptor: u32, buffers: Vec<Vec<u8>>, position: Option<u64>) -> FsResult<u32> {
        granted!(self, "writev");
        Ok(self.with_file(descriptor, "write", |open| {
            if let Some(position) = position.filter(|_| !open.append) {
                open.file.seek(SeekFrom::Start(position))?;
            }
            let mut total = 0u32;
            for buffer in &buffers {
                open.file.write_all(buffer)?;
                total += buffer.len() as u32;
            }
            Ok(total)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_flags_follow_node() {
        assert_eq!(parse_flags("r"), Some(libc::O_RDONLY));
        assert_eq!(parse_flags("w"), Some(libc::O_TRUNC | libc::O_CREAT | libc::O_WRONLY));
        assert_eq!(parse_flags("ax+"), Some(libc::O_APPEND | libc::O_CREAT | libc::O_RDWR | libc::O_EXCL));
        assert_eq!(parse_flags("rw"), None);
    }

    #[test]
    fn modes_may_be_octal_strings() {
        assert_eq!(parse_mode(&Mode::Symbolic("755".into())).unwrap(), 0o755);
        assert_eq!(parse_mode(&Mode::Number(0o600)).unwrap(), 0o600);
        assert_eq!(
            parse_mode(&Mode::Symbolic("rwx".into())).unwrap_err().code.as_deref(),
            Some("ERR_INVALID_ARG_VALUE")
        );
    }

    #[test]
    fn glob_segments_match_like_a_shell() {
        let matches = |pattern: &str, name: &str| {
            segment_matches(&pattern.chars().collect::<Vec<_>>(), &name.chars().collect::<Vec<_>>())
        };
        assert!(matches("*.txt", "a.txt"));
        assert!(!matches("*.txt", "a.txt.bak"));
        assert!(matches("file-?", "file-1"));
        assert!(matches("[a-c]x", "bx"));
        assert!(!matches("[!a-c]x", "bx"));
        assert_eq!(expand_braces("a.{js,ts}"), ["a.js", "a.ts"]);
        assert!(glob_matches("**/node_modules/**", "pkg/node_modules/x/index.js"));
        assert!(!glob_matches("*.md", "docs/readme.md"));
    }

    #[test]
    fn file_urls_decode_to_paths() {
        assert_eq!(percent_decode("/tmp/a%20b"), b"/tmp/a b");
        assert_eq!(percent_decode("/tmp/100%"), b"/tmp/100%");
    }

    #[test]
    fn rm_and_cp_word_their_own_errors() {
        let error =
            system_error("ERR_FS_EISDIR", "Path is a directory", "rm", errno::from_errno(libc::EISDIR), "/tmp/dir");
        assert_eq!(error.name, "SystemError");
        assert_eq!(error.message, "Path is a directory: rm returned EISDIR (is a directory) /tmp/dir");
    }
}
