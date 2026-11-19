//! `jco:node/sqlite`: `node:sqlite` on the SQLite C API, following Node's `node_sqlite.cc`:
//! the same open flags and defaults, parameter binding rules (bare named parameters, anonymous
//! positions skipping named ones), value conversions (numbers unless `readBigInts`, with Node's
//! range check), and error shapes (`ERR_SQLITE_ERROR` with `errcode`/`errstr`,
//! `ERR_INVALID_STATE` for closed databases and finalized statements).

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
    sync::{Arc, Mutex},
};

use libsqlite3_sys as ffi;
use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::sqlite::{
        BackupOptions, Bindings, Changes, Column, DatabaseOptions, Error, Host, HostCursor, HostDatabase, HostSession,
        HostStatement, HostTagStore, LimitName, Path, Row, SessionOptions, StatementOptions, Value,
    },
    hosts::denied_message,
    plugin::NodeView,
    state::{Capability, lock},
};

type SqliteResult<T> = wasmtime::Result<Result<T, Error>>;

const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

// ---- Errors -----------------------------------------------------------------------------------

fn coded(name: &str, code: &str, message: impl Into<String>) -> Error {
    Error { name: name.into(), message: message.into(), code: Some(code.into()), errcode: None, errstr: None }
}

fn invalid_state(message: &str) -> Error {
    coded("Error", "ERR_INVALID_STATE", message)
}

fn not_open() -> Error {
    invalid_state("database is not open")
}

fn finalized() -> Error {
    invalid_state("statement has been finalized")
}

fn errstr(code: c_int) -> String {
    // SAFETY: sqlite3_errstr returns a static string for any code.
    unsafe { CStr::from_ptr(ffi::sqlite3_errstr(code)) }.to_string_lossy().into_owned()
}

/// `ERR_SQLITE_ERROR` from the connection's last failure, as Node's `THROW_ERR_SQLITE_ERROR`.
fn sqlite_error(db: *mut ffi::sqlite3, fallback: c_int) -> Error {
    let (code, message) = if db.is_null() {
        (fallback, errstr(fallback))
    } else {
        // SAFETY: a live connection.
        let code = unsafe { ffi::sqlite3_extended_errcode(db) };
        // SAFETY: a live connection; the message stays valid until the next call on it.
        let message = unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }.to_string_lossy().into_owned();
        (code, message)
    };
    Error {
        name: "Error".into(),
        message,
        code: Some("ERR_SQLITE_ERROR".into()),
        errcode: Some(code),
        errstr: Some(errstr(code)),
    }
}

fn too_large(value: i64) -> Error {
    coded("RangeError", "ERR_OUT_OF_RANGE", format!("Value is too large to be represented as a JavaScript number: {value}"))
}

fn text(pointer: *const c_char) -> Option<String> {
    // SAFETY: SQLite hands back NUL-terminated strings or null.
    (!pointer.is_null()).then(|| unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned())
}

fn c_string(value: &str) -> Result<CString, Error> {
    CString::new(value).map_err(|_| {
        coded("TypeError", "ERR_INVALID_ARG_VALUE", "The argument must not contain null bytes")
    })
}

// ---- Connection state -------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Options {
    read_big_ints: bool,
    return_arrays: bool,
    allow_bare_named_parameters: bool,
    allow_unknown_named_parameters: bool,
}

impl Options {
    fn merged(self, overrides: &StatementOptions) -> Self {
        Self {
            read_big_ints: overrides.read_big_ints.unwrap_or(self.read_big_ints),
            return_arrays: overrides.return_arrays.unwrap_or(self.return_arrays),
            allow_bare_named_parameters: overrides.allow_bare_named_parameters.unwrap_or(self.allow_bare_named_parameters),
            allow_unknown_named_parameters: overrides
                .allow_unknown_named_parameters
                .unwrap_or(self.allow_unknown_named_parameters),
        }
    }
}

struct Connection {
    db: *mut ffi::sqlite3,
    target: (Vec<u8>, bool),
    open_options: DatabaseOptions,
    defaults: Options,
    allow_extension: bool,
    load_extension_enabled: bool,
    statements: HashMap<u64, *mut ffi::sqlite3_stmt>,
    sessions: HashMap<u64, *mut ffi::sqlite3_session>,
    next: u64,
}

// SAFETY: the raw handles are only touched with the connection's mutex held, and SQLite is built
// in its default serialized threading mode.
unsafe impl Send for Connection {}

type Shared = Arc<Mutex<Connection>>;

impl Connection {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn db(&self) -> Result<*mut ffi::sqlite3, Error> {
        if self.db.is_null() { Err(not_open()) } else { Ok(self.db) }
    }

    fn open(&mut self) -> Result<(), Error> {
        if !self.db.is_null() {
            return Err(invalid_state("database is already open"));
        }
        let options = &self.open_options;
        let mut flags = if options.read_only == Some(true) {
            ffi::SQLITE_OPEN_READONLY
        } else {
            ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE
        };
        if self.target.1 {
            flags |= ffi::SQLITE_OPEN_URI;
        }
        let path = CString::new(self.target.0.clone()).map_err(|_| {
            coded("TypeError", "ERR_INVALID_ARG_TYPE", "The \"path\" argument must be a string, Uint8Array, or URL without null bytes.")
        })?;
        let mut db = ptr::null_mut();
        // SAFETY: valid path and out-pointer.
        let status = unsafe { ffi::sqlite3_open_v2(path.as_ptr(), &mut db, flags, ptr::null()) };
        if status != ffi::SQLITE_OK {
            let error = sqlite_error(db, status);
            // SAFETY: open hands back a handle to close even on failure.
            unsafe { ffi::sqlite3_close(db) };
            return Err(error);
        }
        let config = |op: c_int, on: bool| {
            // SAFETY: a live connection and a boolean option with an ignored out-pointer.
            unsafe { ffi::sqlite3_db_config(db, op, c_int::from(on), ptr::null_mut::<c_int>()) }
        };
        config(ffi::SQLITE_DBCONFIG_ENABLE_FKEY, options.enable_foreign_key_constraints.unwrap_or(true));
        let dqs = options.enable_double_quoted_string_literals.unwrap_or(false);
        config(ffi::SQLITE_DBCONFIG_DQS_DML, dqs);
        config(ffi::SQLITE_DBCONFIG_DQS_DDL, dqs);
        config(ffi::SQLITE_DBCONFIG_DEFENSIVE, options.defensive.unwrap_or(true));
        if self.allow_extension {
            config(ffi::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, true);
            self.load_extension_enabled = true;
        }
        if let Some(timeout) = options.timeout {
            // SAFETY: a live connection.
            unsafe { ffi::sqlite3_busy_timeout(db, timeout as c_int) };
        }
        for (name, value) in &options.limits {
            // SAFETY: a live connection and a valid limit id.
            unsafe { ffi::sqlite3_limit(db, limit_id(*name), *value as c_int) };
        }
        self.db = db;
        Ok(())
    }

    fn close(&mut self) -> Result<(), Error> {
        let db = self.db()?;
        for (_, statement) in self.statements.drain() {
            // SAFETY: each statement was prepared on this connection and is finalized once.
            unsafe { ffi::sqlite3_finalize(statement) };
        }
        for (_, session) in self.sessions.drain() {
            // SAFETY: each session was created on this connection and is deleted once.
            unsafe { ffi::sqlite3session_delete(session) };
        }
        // SAFETY: nothing else refers to the handle now.
        let status = unsafe { ffi::sqlite3_close(db) };
        self.db = ptr::null_mut();
        if status != ffi::SQLITE_OK {
            return Err(sqlite_error(ptr::null_mut(), status));
        }
        Ok(())
    }

    fn prepare(&mut self, sql: &str) -> Result<u64, Error> {
        let db = self.db()?;
        let mut statement = ptr::null_mut();
        // SAFETY: a live connection; the SQL is passed with its length.
        let status = unsafe {
            ffi::sqlite3_prepare_v2(db, sql.as_ptr().cast(), sql.len() as c_int, &mut statement, ptr::null_mut())
        };
        if status != ffi::SQLITE_OK {
            return Err(sqlite_error(db, status));
        }
        let id = self.id();
        self.statements.insert(id, statement);
        Ok(id)
    }

    fn statement(&self, id: u64) -> Result<*mut ffi::sqlite3_stmt, Error> {
        self.db()?;
        self.statements.get(&id).copied().ok_or_else(finalized)
    }

    fn finalize(&mut self, id: u64) {
        if let Some(statement) = self.statements.remove(&id) {
            // SAFETY: prepared on this connection and removed from the registry first.
            unsafe { ffi::sqlite3_finalize(statement) };
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.db.is_null() {
            let _ = self.close();
        }
    }
}

fn limit_id(name: LimitName) -> c_int {
    match name {
        LimitName::Length => ffi::SQLITE_LIMIT_LENGTH,
        LimitName::SqlLength => ffi::SQLITE_LIMIT_SQL_LENGTH,
        LimitName::Column => ffi::SQLITE_LIMIT_COLUMN,
        LimitName::ExprDepth => ffi::SQLITE_LIMIT_EXPR_DEPTH,
        LimitName::CompoundSelect => ffi::SQLITE_LIMIT_COMPOUND_SELECT,
        LimitName::VdbeOp => ffi::SQLITE_LIMIT_VDBE_OP,
        LimitName::FunctionArg => ffi::SQLITE_LIMIT_FUNCTION_ARG,
        LimitName::Attach => ffi::SQLITE_LIMIT_ATTACHED,
        LimitName::LikePatternLength => ffi::SQLITE_LIMIT_LIKE_PATTERN_LENGTH,
        LimitName::VariableNumber => ffi::SQLITE_LIMIT_VARIABLE_NUMBER,
        LimitName::TriggerDepth => ffi::SQLITE_LIMIT_TRIGGER_DEPTH,
    }
}

// ---- Statements -------------------------------------------------------------------------------

fn bind_value(db: *mut ffi::sqlite3, statement: *mut ffi::sqlite3_stmt, index: c_int, value: &Value) -> Result<(), Error> {
    // SAFETY: a live statement; text and blobs are copied by SQLite (SQLITE_TRANSIENT).
    let status = unsafe {
        match value {
            Value::Null => ffi::sqlite3_bind_null(statement, index),
            Value::Number(number) => ffi::sqlite3_bind_double(statement, index, *number),
            Value::Bigint(number) => ffi::sqlite3_bind_int64(statement, index, *number),
            Value::Text(text) => ffi::sqlite3_bind_text64(
                statement,
                index,
                text.as_ptr().cast(),
                text.len() as u64,
                ffi::SQLITE_TRANSIENT(),
                ffi::SQLITE_UTF8 as u8,
            ),
            Value::Blob(bytes) => {
                ffi::sqlite3_bind_blob64(statement, index, bytes.as_ptr().cast(), bytes.len() as u64, ffi::SQLITE_TRANSIENT())
            }
        }
    };
    if status != ffi::SQLITE_OK {
        return Err(sqlite_error(db, status));
    }
    Ok(())
}

/// Bind as Node's `StatementSync::BindParams` does.
fn bind(db: *mut ffi::sqlite3, statement: *mut ffi::sqlite3_stmt, options: Options, bindings: &Bindings) -> Result<(), Error> {
    // SAFETY: a live statement.
    unsafe { ffi::sqlite3_clear_bindings(statement) };
    // SAFETY: a live statement.
    let count = unsafe { ffi::sqlite3_bind_parameter_count(statement) };
    let name_of = |index: c_int| {
        // SAFETY: a live statement and an index within its parameter count.
        text(unsafe { ffi::sqlite3_bind_parameter_name(statement, index) })
    };
    if let Some(named) = &bindings.named {
        // Parameters by their name without the `$`, `:` or `@` prefix.
        let mut bare: HashMap<String, String> = HashMap::new();
        if options.allow_bare_named_parameters {
            for index in 1..=count {
                let Some(full) = name_of(index) else { continue };
                let stripped = full[1..].to_owned();
                if let Some(existing) = bare.get(&stripped) {
                    if existing != &full {
                        return Err(invalid_state(&format!(
                            "Cannot create bare named parameter '{stripped}' because of conflicting names '{existing}' and '{full}'."
                        )));
                    }
                }
                bare.insert(stripped, full);
            }
        }
        for (key, value) in named {
            let c_key = c_string(key)?;
            // SAFETY: a live statement and a NUL-terminated name.
            let mut index = unsafe { ffi::sqlite3_bind_parameter_index(statement, c_key.as_ptr()) };
            if index == 0 && options.allow_bare_named_parameters {
                if let Some(full) = bare.get(key) {
                    let c_full = c_string(full)?;
                    // SAFETY: as above.
                    index = unsafe { ffi::sqlite3_bind_parameter_index(statement, c_full.as_ptr()) };
                }
            }
            if index == 0 {
                if options.allow_unknown_named_parameters {
                    continue;
                }
                return Err(invalid_state(&format!("Unknown named parameter '{key}'")));
            }
            bind_value(db, statement, index, value)?;
        }
    }
    let mut anonymous = 1;
    for value in &bindings.positional {
        while anonymous <= count && name_of(anonymous).is_some() {
            anonymous += 1;
        }
        bind_value(db, statement, anonymous, value)?;
        anonymous += 1;
    }
    Ok(())
}

fn column_value(statement: *mut ffi::sqlite3_stmt, index: c_int, options: Options) -> Result<Value, Error> {
    // SAFETY: a statement positioned on a row and a column within its count.
    unsafe {
        Ok(match ffi::sqlite3_column_type(statement, index) {
            ffi::SQLITE_INTEGER => {
                let value = ffi::sqlite3_column_int64(statement, index);
                if options.read_big_ints {
                    Value::Bigint(value)
                } else if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
                    return Err(too_large(value));
                } else {
                    Value::Number(value as f64)
                }
            }
            ffi::SQLITE_FLOAT => Value::Number(ffi::sqlite3_column_double(statement, index)),
            ffi::SQLITE_TEXT => {
                let data = ffi::sqlite3_column_text(statement, index);
                let length = ffi::sqlite3_column_bytes(statement, index) as usize;
                let bytes = if data.is_null() { &[][..] } else { std::slice::from_raw_parts(data, length) };
                Value::Text(String::from_utf8_lossy(bytes).into_owned())
            }
            ffi::SQLITE_BLOB => {
                let data = ffi::sqlite3_column_blob(statement, index);
                let length = ffi::sqlite3_column_bytes(statement, index) as usize;
                Value::Blob(if data.is_null() { Vec::new() } else { std::slice::from_raw_parts(data.cast::<u8>(), length).to_vec() })
            }
            _ => Value::Null,
        })
    }
}

fn read_row(statement: *mut ffi::sqlite3_stmt, options: Options) -> Result<Row, Error> {
    // SAFETY: a live statement.
    let count = unsafe { ffi::sqlite3_column_count(statement) };
    if options.return_arrays {
        return (0..count).map(|index| column_value(statement, index, options)).collect::<Result<_, _>>().map(Row::Array);
    }
    (0..count)
        .map(|index| {
            // SAFETY: a column within the statement's count.
            let name = text(unsafe { ffi::sqlite3_column_name(statement, index) }).unwrap_or_default();
            Ok((name, column_value(statement, index, options)?))
        })
        .collect::<Result<_, _>>()
        .map(Row::Object)
}

fn reset(db: *mut ffi::sqlite3, statement: *mut ffi::sqlite3_stmt) -> Result<(), Error> {
    // SAFETY: a live statement.
    let status = unsafe { ffi::sqlite3_reset(statement) };
    if status != ffi::SQLITE_OK { Err(sqlite_error(db, status)) } else { Ok(()) }
}

fn step(statement: *mut ffi::sqlite3_stmt) -> c_int {
    // SAFETY: a live statement.
    unsafe { ffi::sqlite3_step(statement) }
}

fn prepared(connection: &Connection, id: u64, options: Options, bindings: &Bindings) -> Result<(*mut ffi::sqlite3, *mut ffi::sqlite3_stmt), Error> {
    let statement = connection.statement(id)?;
    let db = connection.db;
    reset(db, statement)?;
    bind(db, statement, options, bindings)?;
    Ok((db, statement))
}

fn all(connection: &Connection, id: u64, options: Options, bindings: &Bindings) -> Result<Vec<Row>, Error> {
    let (db, statement) = prepared(connection, id, options, bindings)?;
    let mut rows = Vec::new();
    let outcome = loop {
        match step(statement) {
            ffi::SQLITE_ROW => match read_row(statement, options) {
                Ok(row) => rows.push(row),
                Err(error) => break Err(error),
            },
            ffi::SQLITE_DONE => break Ok(()),
            status => break Err(sqlite_error(db, status)),
        }
    };
    // SAFETY: a live statement.
    unsafe { ffi::sqlite3_reset(statement) };
    outcome.map(|()| rows)
}

fn get(connection: &Connection, id: u64, options: Options, bindings: &Bindings) -> Result<Option<Row>, Error> {
    let (db, statement) = prepared(connection, id, options, bindings)?;
    let outcome = match step(statement) {
        ffi::SQLITE_ROW => read_row(statement, options).map(Some),
        ffi::SQLITE_DONE => Ok(None),
        status => Err(sqlite_error(db, status)),
    };
    // SAFETY: a live statement.
    unsafe { ffi::sqlite3_reset(statement) };
    outcome
}

fn run(connection: &Connection, id: u64, options: Options, bindings: &Bindings) -> Result<Changes, Error> {
    let (db, statement) = prepared(connection, id, options, bindings)?;
    step(statement);
    reset(db, statement)?;
    // SAFETY: a live connection.
    let (changes, rowid) = unsafe { (ffi::sqlite3_changes64(db), ffi::sqlite3_last_insert_rowid(db)) };
    let number = |value: i64| if options.read_big_ints { Value::Bigint(value) } else { Value::Number(value as f64) };
    Ok(Changes { changes: number(changes), last_insert_rowid: number(rowid) })
}

pub struct SqliteDatabase {
    shared: Shared,
}

pub struct SqliteStatement {
    shared: Shared,
    id: u64,
    options: Options,
}

impl Drop for SqliteStatement {
    fn drop(&mut self) {
        lock(&self.shared).finalize(self.id);
    }
}

pub struct SqliteCursor {
    shared: Shared,
    id: u64,
    options: Options,
    done: bool,
}

pub struct SqliteSession {
    shared: Shared,
    id: u64,
}

pub struct SqliteTagStore {
    shared: Shared,
    capacity: usize,
    /// Prepared statements by SQL, least recently used first.
    cache: Vec<(String, u64)>,
}

impl SqliteTagStore {
    fn statement(&mut self, parts: &[String]) -> Result<u64, Error> {
        let sql = parts.join("?");
        if let Some(position) = self.cache.iter().position(|(cached, _)| *cached == sql) {
            let entry = self.cache.remove(position);
            let id = entry.1;
            self.cache.push(entry);
            return Ok(id);
        }
        let mut connection = lock(&self.shared);
        let id = connection.prepare(&sql)?;
        self.cache.push((sql, id));
        while self.cache.len() > self.capacity {
            let (_, evicted) = self.cache.remove(0);
            connection.finalize(evicted);
        }
        Ok(id)
    }

    fn clear(&mut self) {
        let mut connection = lock(&self.shared);
        for (_, id) in self.cache.drain(..) {
            connection.finalize(id);
        }
    }
}

impl Drop for SqliteTagStore {
    fn drop(&mut self) {
        self.clear();
    }
}

fn positional(values: Vec<Value>) -> Bindings {
    Bindings { named: None, positional: values }
}

fn path_target(path: Path) -> Result<(Vec<u8>, bool), Error> {
    match path {
        Path::Text(text) => Ok((text.into_bytes(), false)),
        Path::Bytes(bytes) => Ok((bytes, false)),
        Path::Url(url) if url.starts_with("file:") => Ok((url.into_bytes(), true)),
        Path::Url(_) => Err(coded("TypeError", "ERR_INVALID_URL_SCHEME", "The URL must be of scheme file:")),
    }
}

// ---- Host -------------------------------------------------------------------------------------

impl Host for NodeView<'_> {
    async fn open_database(&mut self, path: Path, options: DatabaseOptions) -> SqliteResult<Resource<SqliteDatabase>> {
        if !self.node.allows(Capability::Sqlite) {
            return Ok(Err(coded("Error", &Capability::Sqlite.denied_code(), denied_message(Capability::Sqlite))));
        }
        let target = match path_target(path) {
            Ok(target) => target,
            Err(error) => return Ok(Err(error)),
        };
        let defaults = Options {
            read_big_ints: false,
            return_arrays: false,
            allow_bare_named_parameters: true,
            allow_unknown_named_parameters: false,
        }
        .merged(&options.statement);
        let open = options.open.unwrap_or(true);
        let mut connection = Connection {
            db: ptr::null_mut(),
            target,
            allow_extension: options.allow_extension.unwrap_or(false),
            open_options: options,
            defaults,
            load_extension_enabled: false,
            statements: HashMap::new(),
            sessions: HashMap::new(),
            next: 0,
        };
        if open {
            if let Err(error) = connection.open() {
                return Ok(Err(error));
            }
        }
        Ok(Ok(self.table.push(SqliteDatabase { shared: Arc::new(Mutex::new(connection)) })?))
    }

    async fn backup(&mut self, source: Resource<SqliteDatabase>, path: Path, options: BackupOptions) -> SqliteResult<f64> {
        let shared = self.table.get(&source)?.shared.clone();
        let connection = lock(&shared);
        let source_db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        let (target, uri) = match path_target(path) {
            Ok(target) => target,
            Err(error) => return Ok(Err(error)),
        };
        let outcome = (|| -> Result<f64, Error> {
            let path = CString::new(target).map_err(|_| coded("TypeError", "ERR_INVALID_ARG_TYPE", "Invalid path"))?;
            let source_name = c_string(options.source.as_deref().unwrap_or("main"))?;
            let target_name = c_string(options.target.as_deref().unwrap_or("main"))?;
            let rate = options.rate.map_or(100, |rate| rate as c_int);
            let mut flags = ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE;
            if uri {
                flags |= ffi::SQLITE_OPEN_URI;
            }
            let mut destination = ptr::null_mut();
            // SAFETY: valid path and out-pointer; the handle is closed below.
            let status = unsafe { ffi::sqlite3_open_v2(path.as_ptr(), &mut destination, flags, ptr::null()) };
            let close = |db| {
                // SAFETY: closed once.
                unsafe { ffi::sqlite3_close(db) }
            };
            if status != ffi::SQLITE_OK {
                let error = sqlite_error(destination, status);
                close(destination);
                return Err(error);
            }
            // SAFETY: both connections are live.
            let backup = unsafe { ffi::sqlite3_backup_init(destination, target_name.as_ptr(), source_db, source_name.as_ptr()) };
            if backup.is_null() {
                let error = sqlite_error(destination, ffi::SQLITE_ERROR);
                close(destination);
                return Err(error);
            }
            let (status, pages) = loop {
                // SAFETY: a live backup.
                let status = unsafe { ffi::sqlite3_backup_step(backup, rate) };
                // SAFETY: a live backup.
                let pages = unsafe { ffi::sqlite3_backup_pagecount(backup) };
                if !matches!(status, ffi::SQLITE_OK | ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED) {
                    break (status, pages);
                }
            };
            // SAFETY: finished once.
            unsafe { ffi::sqlite3_backup_finish(backup) };
            let outcome = if status == ffi::SQLITE_DONE { Ok(f64::from(pages)) } else { Err(sqlite_error(destination, status)) };
            close(destination);
            outcome
        })();
        Ok(outcome)
    }
}

impl HostDatabase for NodeView<'_> {
    async fn open(&mut self, self_: Resource<SqliteDatabase>) -> SqliteResult<()> {
        Ok(lock(&self.table.get(&self_)?.shared).open())
    }

    async fn close(&mut self, self_: Resource<SqliteDatabase>) -> SqliteResult<()> {
        Ok(lock(&self.table.get(&self_)?.shared).close())
    }

    async fn is_open(&mut self, self_: Resource<SqliteDatabase>) -> SqliteResult<bool> {
        Ok(Ok(!lock(&self.table.get(&self_)?.shared).db.is_null()))
    }

    async fn is_transaction(&mut self, self_: Resource<SqliteDatabase>) -> SqliteResult<bool> {
        let connection = lock(&self.table.get(&self_)?.shared);
        // SAFETY: a live connection.
        Ok(connection.db().map(|db| unsafe { ffi::sqlite3_get_autocommit(db) } == 0))
    }

    async fn exec(&mut self, self_: Resource<SqliteDatabase>, sql: String) -> SqliteResult<()> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        let sql = match c_string(&sql) {
            Ok(sql) => sql,
            Err(error) => return Ok(Err(error)),
        };
        let mut message = ptr::null_mut();
        // SAFETY: a live connection and NUL-terminated SQL; the error message is freed below.
        let status = unsafe { ffi::sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), &mut message) };
        if !message.is_null() {
            // SAFETY: allocated by SQLite.
            unsafe { ffi::sqlite3_free(message.cast()) };
        }
        Ok(if status == ffi::SQLITE_OK { Ok(()) } else { Err(sqlite_error(db, status)) })
    }

    async fn prepare(
        &mut self,
        self_: Resource<SqliteDatabase>,
        sql: String,
        options: StatementOptions,
    ) -> SqliteResult<Resource<SqliteStatement>> {
        let shared = self.table.get(&self_)?.shared.clone();
        let prepared = {
            let mut connection = lock(&shared);
            let options = connection.defaults.merged(&options);
            connection.prepare(&sql).map(|id| (id, options))
        };
        Ok(match prepared {
            Ok((id, options)) => Ok(self.table.push(SqliteStatement { shared, id, options })?),
            Err(error) => Err(error),
        })
    }

    async fn location(&mut self, self_: Resource<SqliteDatabase>, db_name: Option<String>) -> SqliteResult<Option<String>> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        let name = match c_string(db_name.as_deref().unwrap_or("main")) {
            Ok(name) => name,
            Err(error) => return Ok(Err(error)),
        };
        // SAFETY: a live connection and a NUL-terminated schema name.
        let file = text(unsafe { ffi::sqlite3_db_filename(db, name.as_ptr()) });
        Ok(Ok(file.filter(|file| !file.is_empty())))
    }

    async fn get_limit(&mut self, self_: Resource<SqliteDatabase>, name: LimitName) -> SqliteResult<f64> {
        let connection = lock(&self.table.get(&self_)?.shared);
        // SAFETY: a live connection; -1 only reads the limit.
        Ok(connection.db().map(|db| f64::from(unsafe { ffi::sqlite3_limit(db, limit_id(name), -1) })))
    }

    async fn set_limit(&mut self, self_: Resource<SqliteDatabase>, name: LimitName, value: f64) -> SqliteResult<()> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        if value.is_nan() || value < 0.0 || (value.is_finite() && value.fract() != 0.0) {
            return Ok(Err(coded(
                "RangeError",
                "ERR_OUT_OF_RANGE",
                format!("The value of \"value\" is out of range. It must be a non-negative integer. Received {value}"),
            )));
        }
        // Infinity resets a limit to SQLite's compiled-in maximum.
        let value = if value.is_infinite() { c_int::MAX } else { value.min(f64::from(c_int::MAX)) as c_int };
        // SAFETY: a live connection and a valid limit id.
        unsafe { ffi::sqlite3_limit(db, limit_id(name), value) };
        Ok(Ok(()))
    }

    async fn serialize(&mut self, self_: Resource<SqliteDatabase>, db_name: Option<String>) -> SqliteResult<Vec<u8>> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        let name = match c_string(db_name.as_deref().unwrap_or("main")) {
            Ok(name) => name,
            Err(error) => return Ok(Err(error)),
        };
        let mut size: ffi::sqlite3_int64 = 0;
        // SAFETY: a live connection; the image is copied, then freed.
        let image = unsafe { ffi::sqlite3_serialize(db, name.as_ptr(), &mut size, 0) };
        if image.is_null() {
            return Ok(if size == 0 { Ok(Vec::new()) } else { Err(sqlite_error(db, ffi::SQLITE_NOMEM)) });
        }
        // SAFETY: SQLite reports the image's length in `size`.
        let bytes = unsafe { std::slice::from_raw_parts(image, size as usize) }.to_vec();
        // SAFETY: allocated by SQLite.
        unsafe { ffi::sqlite3_free(image.cast()) };
        Ok(Ok(bytes))
    }

    async fn deserialize(&mut self, self_: Resource<SqliteDatabase>, bytes: Vec<u8>, db_name: Option<String>) -> SqliteResult<()> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        let name = match c_string(db_name.as_deref().unwrap_or("main")) {
            Ok(name) => name,
            Err(error) => return Ok(Err(error)),
        };
        // SAFETY: SQLite takes ownership of the buffer (FREEONCLOSE), so it comes from its allocator.
        let status = unsafe {
            let buffer = ffi::sqlite3_malloc64(bytes.len().max(1) as u64).cast::<u8>();
            if buffer.is_null() {
                return Ok(Err(sqlite_error(db, ffi::SQLITE_NOMEM)));
            }
            ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
            ffi::sqlite3_deserialize(
                db,
                name.as_ptr(),
                buffer,
                bytes.len() as i64,
                bytes.len() as i64,
                (ffi::SQLITE_DESERIALIZE_FREEONCLOSE | ffi::SQLITE_DESERIALIZE_RESIZEABLE) as u32,
            )
        };
        Ok(if status == ffi::SQLITE_OK { Ok(()) } else { Err(sqlite_error(db, status)) })
    }

    async fn create_session(&mut self, self_: Resource<SqliteDatabase>, options: SessionOptions) -> SqliteResult<Resource<SqliteSession>> {
        let shared = self.table.get(&self_)?.shared.clone();
        let created = {
            let mut connection = lock(&shared);
            (|| -> Result<u64, Error> {
                let db = connection.db()?;
                let schema = c_string(options.db.as_deref().unwrap_or("main"))?;
                let table = options.table.as_deref().map(c_string).transpose()?;
                let mut session = ptr::null_mut();
                // SAFETY: a live connection and NUL-terminated names.
                let status = unsafe { ffi::sqlite3session_create(db, schema.as_ptr(), &mut session) };
                if status != ffi::SQLITE_OK {
                    return Err(sqlite_error(db, status));
                }
                let table_pointer = table.as_ref().map_or(ptr::null(), |table| table.as_ptr());
                // SAFETY: a live session; null attaches every table.
                let status = unsafe { ffi::sqlite3session_attach(session, table_pointer) };
                if status != ffi::SQLITE_OK {
                    // SAFETY: deleted once.
                    unsafe { ffi::sqlite3session_delete(session) };
                    return Err(sqlite_error(db, status));
                }
                let id = connection.id();
                connection.sessions.insert(id, session);
                Ok(id)
            })()
        };
        Ok(match created {
            Ok(id) => Ok(self.table.push(SqliteSession { shared, id })?),
            Err(error) => Err(error),
        })
    }

    async fn apply_changeset(&mut self, self_: Resource<SqliteDatabase>, bytes: Vec<u8>) -> SqliteResult<bool> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        unsafe extern "C" fn abort(_: *mut c_void, _: c_int, _: *mut ffi::sqlite3_changeset_iter) -> c_int {
            ffi::SQLITE_CHANGESET_ABORT
        }
        // SAFETY: a live connection and a changeset of the given length; nothing is retained.
        let status = unsafe {
            ffi::sqlite3changeset_apply(
                db,
                bytes.len() as c_int,
                bytes.as_ptr().cast_mut().cast(),
                None,
                Some(abort),
                ptr::null_mut(),
            )
        };
        Ok(match status {
            ffi::SQLITE_OK => Ok(true),
            ffi::SQLITE_ABORT => Ok(false),
            status => Err(sqlite_error(db, status)),
        })
    }

    async fn create_tag_store(&mut self, self_: Resource<SqliteDatabase>, capacity: f64) -> SqliteResult<Resource<SqliteTagStore>> {
        let shared = self.table.get(&self_)?.shared.clone();
        if let Err(error) = lock(&shared).db() {
            return Ok(Err(error));
        }
        Ok(Ok(self.table.push(SqliteTagStore { shared, capacity: capacity.max(0.0) as usize, cache: Vec::new() })?))
    }

    async fn enable_load_extension(&mut self, self_: Resource<SqliteDatabase>, allow: bool) -> SqliteResult<()> {
        let mut connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        if allow && !connection.allow_extension {
            return Ok(Err(invalid_state("Cannot enable extension loading because it was disabled at database construction.")));
        }
        // SAFETY: a live connection and a boolean option.
        unsafe {
            ffi::sqlite3_db_config(db, ffi::SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION, c_int::from(allow), ptr::null_mut::<c_int>())
        };
        connection.load_extension_enabled = allow;
        Ok(Ok(()))
    }

    async fn enable_defensive(&mut self, self_: Resource<SqliteDatabase>, active: bool) -> SqliteResult<()> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        // SAFETY: a live connection and a boolean option.
        unsafe { ffi::sqlite3_db_config(db, ffi::SQLITE_DBCONFIG_DEFENSIVE, c_int::from(active), ptr::null_mut::<c_int>()) };
        Ok(Ok(()))
    }

    async fn load_extension(&mut self, self_: Resource<SqliteDatabase>, path: String, entry_point: Option<String>) -> SqliteResult<()> {
        let connection = lock(&self.table.get(&self_)?.shared);
        let db = match connection.db() {
            Ok(db) => db,
            Err(error) => return Ok(Err(error)),
        };
        if !connection.allow_extension || !connection.load_extension_enabled {
            return Ok(Err(invalid_state("extension loading is not allowed")));
        }
        let outcome = (|| -> Result<(), Error> {
            let path = c_string(&path)?;
            let entry = entry_point.as_deref().map(c_string).transpose()?;
            let mut message = ptr::null_mut();
            // SAFETY: a live connection and NUL-terminated strings; the message is freed below.
            let status = unsafe {
                ffi::sqlite3_load_extension(db, path.as_ptr(), entry.as_ref().map_or(ptr::null(), |entry| entry.as_ptr()), &mut message)
            };
            if status == ffi::SQLITE_OK {
                return Ok(());
            }
            let mut error = sqlite_error(db, status);
            if let Some(text) = text(message) {
                error.message = text;
            }
            // SAFETY: allocated by SQLite (or null).
            unsafe { ffi::sqlite3_free(message.cast()) };
            Err(error)
        })();
        Ok(outcome)
    }

    async fn drop(&mut self, rep: Resource<SqliteDatabase>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostStatement for NodeView<'_> {
    async fn all(&mut self, self_: Resource<SqliteStatement>, parameters: Bindings) -> SqliteResult<Vec<Row>> {
        let statement = self.table.get(&self_)?;
        Ok(all(&lock(&statement.shared), statement.id, statement.options, &parameters))
    }

    async fn get(&mut self, self_: Resource<SqliteStatement>, parameters: Bindings) -> SqliteResult<Option<Row>> {
        let statement = self.table.get(&self_)?;
        Ok(get(&lock(&statement.shared), statement.id, statement.options, &parameters))
    }

    async fn run(&mut self, self_: Resource<SqliteStatement>, parameters: Bindings) -> SqliteResult<Changes> {
        let statement = self.table.get(&self_)?;
        Ok(run(&lock(&statement.shared), statement.id, statement.options, &parameters))
    }

    async fn iterate(&mut self, self_: Resource<SqliteStatement>, parameters: Bindings) -> SqliteResult<Resource<SqliteCursor>> {
        let statement = self.table.get(&self_)?;
        let (shared, id, options) = (statement.shared.clone(), statement.id, statement.options);
        if let Err(error) = prepared(&lock(&shared), id, options, &parameters) {
            return Ok(Err(error));
        }
        Ok(Ok(self.table.push(SqliteCursor { shared, id, options, done: false })?))
    }

    async fn columns(&mut self, self_: Resource<SqliteStatement>) -> SqliteResult<Vec<Column>> {
        let statement = self.table.get(&self_)?;
        let connection = lock(&statement.shared);
        let raw = match connection.statement(statement.id) {
            Ok(raw) => raw,
            Err(error) => return Ok(Err(error)),
        };
        // SAFETY: a live statement and columns within its count.
        let columns = unsafe {
            (0..ffi::sqlite3_column_count(raw))
                .map(|index| Column {
                    column: text(ffi::sqlite3_column_origin_name(raw, index)),
                    database: text(ffi::sqlite3_column_database_name(raw, index)),
                    name: text(ffi::sqlite3_column_name(raw, index)).unwrap_or_default(),
                    table: text(ffi::sqlite3_column_table_name(raw, index)),
                    type_: text(ffi::sqlite3_column_decltype(raw, index)),
                })
                .collect()
        };
        Ok(Ok(columns))
    }

    async fn source_sql(&mut self, self_: Resource<SqliteStatement>) -> SqliteResult<String> {
        let statement = self.table.get(&self_)?;
        let connection = lock(&statement.shared);
        // SAFETY: a live statement.
        Ok(connection.statement(statement.id).map(|raw| text(unsafe { ffi::sqlite3_sql(raw) }).unwrap_or_default()))
    }

    async fn expanded_sql(&mut self, self_: Resource<SqliteStatement>) -> SqliteResult<String> {
        let statement = self.table.get(&self_)?;
        let connection = lock(&statement.shared);
        Ok(connection.statement(statement.id).and_then(|raw| {
            // SAFETY: a live statement; the expansion is freed after copying.
            let expanded = unsafe { ffi::sqlite3_expanded_sql(raw) };
            let sql = text(expanded).ok_or_else(|| sqlite_error(connection.db, ffi::SQLITE_NOMEM))?;
            // SAFETY: allocated by SQLite.
            unsafe { ffi::sqlite3_free(expanded.cast()) };
            Ok(sql)
        }))
    }

    async fn set_read_big_ints(&mut self, self_: Resource<SqliteStatement>, enabled: bool) -> SqliteResult<()> {
        let statement = self.table.get_mut(&self_)?;
        if let Err(error) = lock(&statement.shared).statement(statement.id) {
            return Ok(Err(error));
        }
        statement.options.read_big_ints = enabled;
        Ok(Ok(()))
    }

    async fn set_return_arrays(&mut self, self_: Resource<SqliteStatement>, enabled: bool) -> SqliteResult<()> {
        let statement = self.table.get_mut(&self_)?;
        if let Err(error) = lock(&statement.shared).statement(statement.id) {
            return Ok(Err(error));
        }
        statement.options.return_arrays = enabled;
        Ok(Ok(()))
    }

    async fn set_allow_bare_named_parameters(&mut self, self_: Resource<SqliteStatement>, enabled: bool) -> SqliteResult<()> {
        let statement = self.table.get_mut(&self_)?;
        if let Err(error) = lock(&statement.shared).statement(statement.id) {
            return Ok(Err(error));
        }
        statement.options.allow_bare_named_parameters = enabled;
        Ok(Ok(()))
    }

    async fn set_allow_unknown_named_parameters(&mut self, self_: Resource<SqliteStatement>, enabled: bool) -> SqliteResult<()> {
        let statement = self.table.get_mut(&self_)?;
        if let Err(error) = lock(&statement.shared).statement(statement.id) {
            return Ok(Err(error));
        }
        statement.options.allow_unknown_named_parameters = enabled;
        Ok(Ok(()))
    }

    async fn drop(&mut self, rep: Resource<SqliteStatement>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostCursor for NodeView<'_> {
    async fn next(&mut self, self_: Resource<SqliteCursor>) -> SqliteResult<Option<Row>> {
        let cursor = self.table.get_mut(&self_)?;
        if cursor.done {
            return Ok(Ok(None));
        }
        let connection = lock(&cursor.shared);
        let raw = match connection.statement(cursor.id) {
            Ok(raw) => raw,
            Err(error) => return Ok(Err(error)),
        };
        let outcome = match step(raw) {
            ffi::SQLITE_ROW => read_row(raw, cursor.options).map(Some),
            ffi::SQLITE_DONE => Ok(None),
            status => Err(sqlite_error(connection.db, status)),
        };
        if !matches!(outcome, Ok(Some(_))) {
            cursor.done = true;
            // SAFETY: a live statement.
            unsafe { ffi::sqlite3_reset(raw) };
        }
        Ok(outcome)
    }

    async fn close(&mut self, self_: Resource<SqliteCursor>) -> SqliteResult<()> {
        let cursor = self.table.get_mut(&self_)?;
        if !cursor.done {
            cursor.done = true;
            if let Ok(raw) = lock(&cursor.shared).statement(cursor.id) {
                // SAFETY: a live statement.
                unsafe { ffi::sqlite3_reset(raw) };
            }
        }
        Ok(Ok(()))
    }

    async fn drop(&mut self, rep: Resource<SqliteCursor>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl HostSession for NodeView<'_> {
    async fn changeset(&mut self, self_: Resource<SqliteSession>) -> SqliteResult<Vec<u8>> {
        let session = self.table.get(&self_)?;
        Ok(session_bytes(&lock(&session.shared), session.id, false))
    }

    async fn patchset(&mut self, self_: Resource<SqliteSession>) -> SqliteResult<Vec<u8>> {
        let session = self.table.get(&self_)?;
        Ok(session_bytes(&lock(&session.shared), session.id, true))
    }

    async fn close(&mut self, self_: Resource<SqliteSession>) -> SqliteResult<()> {
        let session = self.table.get(&self_)?;
        let mut connection = lock(&session.shared);
        if let Err(error) = connection.db() {
            return Ok(Err(error));
        }
        match connection.sessions.remove(&session.id) {
            Some(raw) => {
                // SAFETY: removed from the registry first, so deleted once.
                unsafe { ffi::sqlite3session_delete(raw) };
                Ok(Ok(()))
            }
            None => Ok(Err(invalid_state("session is not open"))),
        }
    }

    async fn drop(&mut self, rep: Resource<SqliteSession>) -> wasmtime::Result<()> {
        let session = self.table.delete(rep)?;
        if let Some(raw) = lock(&session.shared).sessions.remove(&session.id) {
            // SAFETY: removed from the registry first, so deleted once.
            unsafe { ffi::sqlite3session_delete(raw) };
        }
        Ok(())
    }
}

fn session_bytes(connection: &Connection, id: u64, patchset: bool) -> Result<Vec<u8>, Error> {
    let db = connection.db()?;
    let session = *connection.sessions.get(&id).ok_or_else(|| invalid_state("session is not open"))?;
    let mut size: c_int = 0;
    let mut data = ptr::null_mut();
    // SAFETY: a live session; the buffer is copied, then freed.
    let status = unsafe {
        if patchset {
            ffi::sqlite3session_patchset(session, &mut size, &mut data)
        } else {
            ffi::sqlite3session_changeset(session, &mut size, &mut data)
        }
    };
    if status != ffi::SQLITE_OK {
        return Err(sqlite_error(db, status));
    }
    let bytes = if data.is_null() {
        Vec::new()
    } else {
        // SAFETY: SQLite reports the buffer's length in `size`.
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) }.to_vec()
    };
    // SAFETY: allocated by SQLite (or null).
    unsafe { ffi::sqlite3_free(data) };
    Ok(bytes)
}

impl HostTagStore for NodeView<'_> {
    async fn all(&mut self, self_: Resource<SqliteTagStore>, parts: Vec<String>, parameters: Vec<Value>) -> SqliteResult<Vec<Row>> {
        let store = self.table.get_mut(&self_)?;
        let id = match store.statement(&parts) {
            Ok(id) => id,
            Err(error) => return Ok(Err(error)),
        };
        let connection = lock(&store.shared);
        let options = connection.defaults;
        Ok(all(&connection, id, options, &positional(parameters)))
    }

    async fn get(&mut self, self_: Resource<SqliteTagStore>, parts: Vec<String>, parameters: Vec<Value>) -> SqliteResult<Option<Row>> {
        let store = self.table.get_mut(&self_)?;
        let id = match store.statement(&parts) {
            Ok(id) => id,
            Err(error) => return Ok(Err(error)),
        };
        let connection = lock(&store.shared);
        let options = connection.defaults;
        Ok(get(&connection, id, options, &positional(parameters)))
    }

    async fn run(&mut self, self_: Resource<SqliteTagStore>, parts: Vec<String>, parameters: Vec<Value>) -> SqliteResult<Changes> {
        let store = self.table.get_mut(&self_)?;
        let id = match store.statement(&parts) {
            Ok(id) => id,
            Err(error) => return Ok(Err(error)),
        };
        let connection = lock(&store.shared);
        let options = connection.defaults;
        Ok(run(&connection, id, options, &positional(parameters)))
    }

    async fn iterate(
        &mut self,
        self_: Resource<SqliteTagStore>,
        parts: Vec<String>,
        parameters: Vec<Value>,
    ) -> SqliteResult<Resource<SqliteCursor>> {
        let store = self.table.get_mut(&self_)?;
        let id = match store.statement(&parts) {
            Ok(id) => id,
            Err(error) => return Ok(Err(error)),
        };
        let shared = store.shared.clone();
        let options = lock(&shared).defaults;
        if let Err(error) = prepared(&lock(&shared), id, options, &positional(parameters)) {
            return Ok(Err(error));
        }
        Ok(Ok(self.table.push(SqliteCursor { shared, id, options, done: false })?))
    }

    async fn size(&mut self, self_: Resource<SqliteTagStore>) -> SqliteResult<f64> {
        Ok(Ok(self.table.get(&self_)?.cache.len() as f64))
    }

    async fn capacity(&mut self, self_: Resource<SqliteTagStore>) -> SqliteResult<f64> {
        Ok(Ok(self.table.get(&self_)?.capacity as f64))
    }

    async fn clear(&mut self, self_: Resource<SqliteTagStore>) -> SqliteResult<()> {
        self.table.get_mut(&self_)?.clear();
        Ok(Ok(()))
    }

    async fn drop(&mut self, rep: Resource<SqliteTagStore>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Shared {
        let mut connection = Connection {
            db: ptr::null_mut(),
            target: (b":memory:".to_vec(), false),
            open_options: DatabaseOptions {
                open: None,
                read_only: None,
                enable_foreign_key_constraints: None,
                enable_double_quoted_string_literals: None,
                allow_extension: None,
                timeout: None,
                defensive: None,
                statement: StatementOptions {
                    read_big_ints: None,
                    return_arrays: None,
                    allow_bare_named_parameters: None,
                    allow_unknown_named_parameters: None,
                },
                limits: Vec::new(),
            },
            defaults: Options {
                read_big_ints: false,
                return_arrays: false,
                allow_bare_named_parameters: true,
                allow_unknown_named_parameters: false,
            },
            allow_extension: false,
            load_extension_enabled: false,
            statements: HashMap::new(),
            sessions: HashMap::new(),
            next: 0,
        };
        connection.open().unwrap();
        Arc::new(Mutex::new(connection))
    }

    #[test]
    fn binds_bare_named_and_anonymous_parameters_like_node() {
        let shared = memory();
        let mut connection = lock(&shared);
        let options = connection.defaults;
        let create = connection.prepare("CREATE TABLE t(id INTEGER PRIMARY KEY, value TEXT)").unwrap();
        run(&connection, create, options, &positional(vec![])).unwrap();
        let insert = connection.prepare("INSERT INTO t VALUES ($id, ?)").unwrap();
        let changes = run(
            &connection,
            insert,
            options,
            &Bindings { named: Some(vec![("id".into(), Value::Number(1.0))]), positional: vec![Value::Text("hello".into())] },
        )
        .unwrap();
        assert!(matches!(changes.last_insert_rowid, Value::Number(n) if n == 1.0));
        let error = run(&connection, insert, options, &Bindings { named: Some(vec![("nope".into(), Value::Null)]), positional: vec![] })
            .unwrap_err();
        assert_eq!(error.message, "Unknown named parameter 'nope'");
        let duplicate = run(
            &connection,
            insert,
            options,
            &Bindings { named: Some(vec![("id".into(), Value::Number(1.0))]), positional: vec![Value::Text("again".into())] },
        )
        .unwrap_err();
        assert_eq!((duplicate.code.as_deref(), duplicate.errcode, duplicate.errstr.as_deref()), (Some("ERR_SQLITE_ERROR"), Some(1555), Some("constraint failed")));
        connection.close().unwrap();
        assert_eq!(all(&connection, insert, options, &positional(vec![])).unwrap_err().message, "database is not open");
    }
}
