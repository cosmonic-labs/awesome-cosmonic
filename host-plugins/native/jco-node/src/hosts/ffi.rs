//! `jco:node/ffi`: native libraries, as Node's `node:ffi` (`--experimental-ffi`) exposes them.
//!
//! Granting this interface lets the guest run arbitrary native code in the host process, exactly
//! as Jco's Node provider does; it is never granted by default.
//!
//! Calls go through the platform C calling convention without libffi: every function is called
//! as one taking eight integer-class and eight floating-point arguments. On the System V x86-64
//! and AArch64 conventions integer and floating-point arguments are assigned to their own
//! register files independently, so the callee reads exactly the arguments its real signature
//! declares and ignores the rest. That covers any non-variadic signature with at most eight of
//! each class; anything wider is refused. Other platforms report the interface unavailable.

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_void},
};

use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::ffi::{Error, Failure, Host, Signature, Value},
    hosts::{denied_trap, denied_variant},
    plugin::NodeView,
    state::{Capability, lock},
};

type FfiResult<T> = wasmtime::Result<Result<T, Error>>;

/// The most arguments of each register class a call may pass.
const REGISTERS: usize = 8;

#[derive(Default)]
pub(crate) struct Libraries {
    next: u32,
    open: HashMap<u32, Library>,
}

struct Library {
    /// The `dlopen` handle, kept as an address so the table can cross threads.
    handle: usize,
    functions: HashMap<String, Function>,
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `dlopen` and is closed exactly once, here.
        unsafe { libc::dlclose(self.handle as *mut c_void) };
    }
}

struct Function {
    address: usize,
    arguments: Vec<Kind>,
    returns: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Void,
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Pointer,
    /// A NUL-terminated string: passed from a guest string, returned as its address (as Node
    /// returns it).
    String,
}

impl Kind {
    fn parse(name: &str) -> Result<Self, Error> {
        Ok(match name {
            "void" => Kind::Void,
            "bool" => Kind::Bool,
            "int8" | "char" => Kind::I8,
            "uint8" => Kind::U8,
            "int16" => Kind::I16,
            "uint16" => Kind::U16,
            "int32" => Kind::I32,
            "uint32" => Kind::U32,
            "int64" => Kind::I64,
            "uint64" => Kind::U64,
            "float" | "float32" => Kind::F32,
            "double" | "float64" => Kind::F64,
            "pointer" => Kind::Pointer,
            "string" => Kind::String,
            other => return Err(Error::UnsupportedType(format!("{other} cannot cross the component boundary"))),
        })
    }

    /// A type `read` and `write` accept: the primitives Node has `get…`/`set…` accessors for.
    fn primitive(name: &str) -> Result<Self, Error> {
        match Kind::parse(name) {
            Ok(kind) if !matches!(kind, Kind::Void | Kind::Bool | Kind::Pointer | Kind::String) => Ok(kind),
            _ => Err(Error::UnsupportedType(format!("{name} is not a primitive node:ffi can read or write"))),
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Kind::F32 | Kind::F64)
    }
}

fn failed(code: &str, message: impl Into<String>) -> Error {
    Error::Failed(Failure { code: code.into(), message: message.into() })
}

fn dl_error() -> String {
    // SAFETY: `dlerror` returns null or a NUL-terminated string valid until the next dl call.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: checked non-null above.
        unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
    }
}

fn lookup(handle: usize, name: &str) -> Result<usize, Error> {
    let symbol = CString::new(name).map_err(|_| failed("ERR_FFI_CALL_FAILED", format!("dlsym failed: invalid symbol name {name:?}")))?;
    // SAFETY: clears any stale error so a null result below can be told apart.
    unsafe { libc::dlerror() };
    // SAFETY: `handle` is a live `dlopen` handle and `symbol` is NUL-terminated.
    let address = unsafe { libc::dlsym(handle as *mut c_void, symbol.as_ptr()) };
    if address.is_null() {
        return Err(failed("ERR_FFI_CALL_FAILED", format!("dlsym failed: {}", dl_error())));
    }
    Ok(address as usize)
}

/// The value as the bits of an integer argument of `kind`, extended as the C convention expects.
fn integer(kind: Kind, value: &Value) -> Result<i64, Error> {
    let raw: i128 = match value {
        Value::Nothing => 0,
        Value::Boolean(value) => i128::from(*value),
        Value::Int8(value) => i128::from(*value),
        Value::Uint8(value) => i128::from(*value),
        Value::Int16(value) => i128::from(*value),
        Value::Uint16(value) => i128::from(*value),
        Value::Int32(value) => i128::from(*value),
        Value::Uint32(value) => i128::from(*value),
        Value::Int64(value) => i128::from(*value),
        Value::Uint64(value) => i128::from(*value),
        Value::Pointer(value) => i128::from(*value),
        Value::Float32(value) => *value as i128,
        Value::Float64(value) => *value as i128,
        Value::Text(_) => return Err(failed("ERR_INVALID_ARG_TYPE", format!("a string cannot be passed as {kind:?}"))),
    };
    Ok(match kind {
        Kind::Bool => i64::from(raw != 0),
        Kind::I8 => i64::from(raw as i8),
        Kind::U8 => i64::from(raw as u8),
        Kind::I16 => i64::from(raw as i16),
        Kind::U16 => i64::from(raw as u16),
        Kind::I32 => i64::from(raw as i32),
        Kind::U32 => i64::from(raw as u32),
        _ => raw as i64,
    })
}

fn float(value: &Value) -> f64 {
    match value {
        Value::Float32(value) => f64::from(*value),
        Value::Float64(value) => *value,
        Value::Int8(value) => f64::from(*value),
        Value::Uint8(value) => f64::from(*value),
        Value::Int16(value) => f64::from(*value),
        Value::Uint16(value) => f64::from(*value),
        Value::Int32(value) => f64::from(*value),
        Value::Uint32(value) => f64::from(*value),
        Value::Int64(value) => *value as f64,
        Value::Uint64(value) => *value as f64,
        Value::Pointer(value) => *value as f64,
        Value::Boolean(value) => f64::from(u8::from(*value)),
        Value::Nothing | Value::Text(_) => 0.0,
    }
}

/// The integer result of a call, as a value of `kind`.
fn from_integer(kind: Kind, raw: i64) -> Value {
    match kind {
        Kind::Void => Value::Nothing,
        Kind::Bool => Value::Boolean(raw as u8 != 0),
        Kind::I8 => Value::Int8(raw as i8),
        Kind::U8 => Value::Uint8(raw as u8),
        Kind::I16 => Value::Int16(raw as i16),
        Kind::U16 => Value::Uint16(raw as u16),
        Kind::I32 => Value::Int32(raw as i32),
        Kind::U32 => Value::Uint32(raw as u32),
        Kind::I64 => Value::Int64(raw),
        Kind::U64 => Value::Uint64(raw as u64),
        Kind::Pointer | Kind::String => Value::Pointer(raw as u64),
        Kind::F32 | Kind::F64 => unreachable!("floating-point results come back in a vector register"),
    }
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod native {
    /// Every call, as the C convention lays it out: the integer-class arguments in their
    /// registers, the floating-point ones in theirs.
    type Integer = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64, f64, f64, f64, f64, f64, f64, f64, f64) -> i64;
    type Float = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64, f64, f64, f64, f64, f64, f64, f64, f64) -> f64;

    pub(super) const AVAILABLE: bool = true;

    /// # Safety
    /// `address` must be a non-variadic C function whose signature has at most eight integer and
    /// eight floating-point arguments, matching `ints` and `floats` in order.
    pub(super) unsafe fn call_integer(address: usize, i: [i64; 8], f: [f64; 8]) -> i64 {
        // SAFETY: the caller's contract; see the module documentation for why the wider
        // signature reaches the callee's real arguments.
        unsafe {
            let function: Integer = std::mem::transmute(address);
            function(i[0], i[1], i[2], i[3], i[4], i[5], i[6], i[7], f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7])
        }
    }

    /// # Safety
    /// As [`call_integer`], for a function returning `double` (or `float`, in the low half).
    pub(super) unsafe fn call_float(address: usize, i: [i64; 8], f: [f64; 8]) -> f64 {
        // SAFETY: as in `call_integer`.
        unsafe {
            let function: Float = std::mem::transmute(address);
            function(i[0], i[1], i[2], i[3], i[4], i[5], i[6], i[7], f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7])
        }
    }
}

#[cfg(not(all(unix, any(target_arch = "x86_64", target_arch = "aarch64"))))]
mod native {
    pub(super) const AVAILABLE: bool = false;
    pub(super) unsafe fn call_integer(_: usize, _: [i64; 8], _: [f64; 8]) -> i64 {
        unreachable!()
    }
    pub(super) unsafe fn call_float(_: usize, _: [i64; 8], _: [f64; 8]) -> f64 {
        unreachable!()
    }
}

/// A `float` argument: its bits in the low half of the vector register, as the callee reads it.
fn f32_in_register(value: f32) -> f64 {
    f64::from_bits(u64::from(value.to_bits()))
}

fn invoke(function: &Function, args: &[Value]) -> Result<Value, Error> {
    if args.len() != function.arguments.len() {
        return Err(failed(
            "ERR_INVALID_ARG_VALUE",
            format!("expected {} arguments, received {}", function.arguments.len(), args.len()),
        ));
    }
    let mut ints = [0i64; REGISTERS];
    let mut floats = [0f64; REGISTERS];
    let (mut next_int, mut next_float) = (0, 0);
    // Strings passed by value live until the call returns.
    let mut strings = Vec::new();
    for (kind, value) in function.arguments.iter().zip(args) {
        match kind {
            Kind::F32 => {
                floats[next_float] = f32_in_register(float(value) as f32);
                next_float += 1;
            }
            Kind::F64 => {
                floats[next_float] = float(value);
                next_float += 1;
            }
            Kind::String => {
                let address = match value {
                    Value::Text(text) => {
                        let text = CString::new(text.as_str())
                            .map_err(|_| failed("ERR_INVALID_ARG_VALUE", "a string argument cannot contain NUL"))?;
                        let address = text.as_ptr() as i64;
                        strings.push(text);
                        address
                    }
                    other => integer(Kind::Pointer, other)?,
                };
                ints[next_int] = address;
                next_int += 1;
            }
            kind => {
                ints[next_int] = integer(*kind, value)?;
                next_int += 1;
            }
        }
    }
    let result = match function.returns {
        // SAFETY: `define` checked the register counts, and granting this interface is the
        // embedder's statement that the guest may call native code with these signatures.
        Kind::F64 => Value::Float64(unsafe { native::call_float(function.address, ints, floats) }),
        Kind::F32 => {
            // SAFETY: as above.
            let raw = unsafe { native::call_float(function.address, ints, floats) };
            Value::Float32(f32::from_bits(raw.to_bits() as u32))
        }
        // SAFETY: as above.
        kind => from_integer(kind, unsafe { native::call_integer(function.address, ints, floats) }),
    };
    drop(strings);
    Ok(result)
}

fn non_null(pointer: u64) -> Result<*mut u8, Error> {
    if pointer == 0 {
        return Err(failed("ERR_FFI_INVALID_POINTER", "Cannot dereference a null pointer"));
    }
    Ok(pointer as usize as *mut u8)
}

fn address(pointer: u64, offset: u64) -> Result<*mut u8, Error> {
    let base = non_null(pointer)?;
    Ok(base.wrapping_add(offset as usize))
}

/// Read one primitive at `at`.
///
/// # Safety
/// `at` must be readable for the size of `kind`.
unsafe fn load(at: *const u8, kind: Kind) -> Value {
    // SAFETY: the caller's contract; reads tolerate any alignment.
    unsafe {
        match kind {
            Kind::I8 => Value::Int8(at.cast::<i8>().read_unaligned()),
            Kind::U8 => Value::Uint8(at.read_unaligned()),
            Kind::I16 => Value::Int16(at.cast::<i16>().read_unaligned()),
            Kind::U16 => Value::Uint16(at.cast::<u16>().read_unaligned()),
            Kind::I32 => Value::Int32(at.cast::<i32>().read_unaligned()),
            Kind::U32 => Value::Uint32(at.cast::<u32>().read_unaligned()),
            Kind::I64 => Value::Int64(at.cast::<i64>().read_unaligned()),
            Kind::U64 => Value::Uint64(at.cast::<u64>().read_unaligned()),
            Kind::F32 => Value::Float32(at.cast::<f32>().read_unaligned()),
            Kind::F64 => Value::Float64(at.cast::<f64>().read_unaligned()),
            _ => unreachable!("only primitives are read"),
        }
    }
}

/// Write one primitive at `at`.
///
/// # Safety
/// `at` must be writable for the size of `kind`.
unsafe fn store(at: *mut u8, kind: Kind, value: &Value) -> Result<(), Error> {
    let bits = if kind.is_float() { 0 } else { integer(kind, value)? };
    // SAFETY: the caller's contract; writes tolerate any alignment.
    unsafe {
        match kind {
            Kind::I8 | Kind::U8 => at.write_unaligned(bits as u8),
            Kind::I16 | Kind::U16 => at.cast::<u16>().write_unaligned(bits as u16),
            Kind::I32 | Kind::U32 => at.cast::<u32>().write_unaligned(bits as u32),
            Kind::I64 | Kind::U64 => at.cast::<u64>().write_unaligned(bits as u64),
            Kind::F32 => at.cast::<f32>().write_unaligned(float(value) as f32),
            Kind::F64 => at.cast::<f64>().write_unaligned(float(value)),
            _ => unreachable!("only primitives are written"),
        }
    }
    Ok(())
}

/// `data` encoded as Node's `exportString` writes it, terminator included.
fn encode_text(data: &str, encoding: &str) -> Result<Vec<u8>, Error> {
    Ok(match encoding.to_ascii_lowercase().as_str() {
        "" | "utf8" | "utf-8" => data.bytes().chain([0]).collect(),
        "latin1" | "binary" | "ascii" => data.chars().map(|c| c as u32 as u8).chain([0]).collect(),
        "utf16le" | "utf-16le" | "ucs2" | "ucs-2" => {
            data.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
        }
        _ => return Err(failed("ERR_UNKNOWN_ENCODING", format!("Unknown encoding: {encoding}"))),
    })
}

/// Something with an address, for `currentEventLoop`: the host's loop is not a libuv loop, but
/// the guest only needs a stable, non-null handle.
static EVENT_LOOP: u8 = 0;

impl NodeView<'_> {
    fn ffi_allowed(&self) -> Result<(), Error> {
        if !self.node.allows(Capability::Ffi) {
            return Err(Error::Denied(denied_variant(Capability::Ffi)));
        }
        if !native::AVAILABLE {
            return Err(Error::Unavailable("native calls are not supported on this platform".into()));
        }
        Ok(())
    }

    fn with_library<T>(&self, handle: u32, body: impl FnOnce(&mut Library) -> Result<T, Error>) -> Result<T, Error> {
        self.ffi_allowed()?;
        let mut libraries = lock(&self.node.libraries);
        let library = libraries.open.get_mut(&handle).ok_or(Error::NoSuchLibrary(handle))?;
        body(library)
    }
}

impl Host for NodeView<'_> {
    async fn suffix(&mut self) -> wasmtime::Result<String> {
        if !self.node.allows(Capability::Ffi) {
            return Err(denied_trap(Capability::Ffi));
        }
        Ok(if cfg!(target_os = "macos") {
            "dylib"
        } else if cfg!(windows) {
            "dll"
        } else {
            "so"
        }
        .into())
    }

    async fn open(&mut self, path: Option<String>) -> FfiResult<u32> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        let path = match path.map(|path| CString::new(self.node.resolve(&path).to_string_lossy().into_owned())).transpose() {
            Ok(path) => path,
            Err(_) => return Ok(Err(failed("ERR_FFI_CALL_FAILED", "dlopen failed: invalid path"))),
        };
        // SAFETY: `path` is null (the process image) or a NUL-terminated path.
        let handle = unsafe {
            libc::dlopen(path.as_ref().map_or(std::ptr::null(), |path| path.as_ptr()), libc::RTLD_NOW | libc::RTLD_LOCAL)
        };
        if handle.is_null() {
            return Ok(Err(failed("ERR_FFI_CALL_FAILED", format!("dlopen failed: {}", dl_error()))));
        }
        let mut libraries = lock(&self.node.libraries);
        libraries.next += 1;
        let id = libraries.next;
        libraries.open.insert(id, Library { handle: handle as usize, functions: HashMap::new() });
        Ok(Ok(id))
    }

    async fn close(&mut self, handle: u32) -> FfiResult<()> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok(match lock(&self.node.libraries).open.remove(&handle) {
            Some(_) => Ok(()),
            None => Err(Error::NoSuchLibrary(handle)),
        })
    }

    async fn symbol(&mut self, handle: u32, name: String) -> FfiResult<u64> {
        Ok(self.with_library(handle, |library| lookup(library.handle, &name).map(|address| address as u64)))
    }

    async fn define(&mut self, handle: u32, name: String, sig: Signature) -> FfiResult<()> {
        Ok(self.with_library(handle, |library| {
            let arguments = sig.arguments.iter().map(|name| Kind::parse(name)).collect::<Result<Vec<_>, _>>()?;
            if arguments.contains(&Kind::Void) {
                return Err(Error::UnsupportedType("void is not an argument type".into()));
            }
            let returns = Kind::parse(&sig.returns)?;
            let floats = arguments.iter().filter(|kind| kind.is_float()).count();
            if floats > REGISTERS || arguments.len() - floats > REGISTERS {
                return Err(Error::UnsupportedType(format!(
                    "a native signature with more than {REGISTERS} integer or {REGISTERS} floating-point arguments"
                )));
            }
            let address = lookup(library.handle, &name)?;
            library.functions.insert(name, Function { address, arguments, returns });
            Ok(())
        }))
    }

    async fn call(&mut self, handle: u32, name: String, args: Vec<Value>) -> FfiResult<Value> {
        Ok(self.with_library(handle, |library| {
            let function = library.functions.get(&name).ok_or_else(|| Error::NoSuchSymbol(name.clone()))?;
            invoke(function, &args)
        }))
    }

    async fn read(&mut self, pointer: u64, offset: u64, kind: String) -> FfiResult<Value> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok((|| {
            let kind = Kind::primitive(&kind)?;
            let at = address(pointer, offset)?;
            // SAFETY: the guest names host memory it obtained from native code; granting ffi is
            // the embedder's acceptance of that, as with Node's own accessors.
            Ok(unsafe { load(at, kind) })
        })())
    }

    async fn write(&mut self, pointer: u64, offset: u64, kind: String, data: Value) -> FfiResult<()> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok((|| {
            let kind = Kind::primitive(&kind)?;
            let at = address(pointer, offset)?;
            // SAFETY: as in `read`.
            unsafe { store(at, kind, &data) }
        })())
    }

    async fn read_text(&mut self, pointer: u64) -> FfiResult<Option<String>> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        if pointer == 0 {
            return Ok(Ok(None));
        }
        // SAFETY: as in `read`; the guest names a NUL-terminated string.
        let text = unsafe { CStr::from_ptr(pointer as usize as *const libc::c_char) };
        Ok(Ok(Some(text.to_string_lossy().into_owned())))
    }

    async fn read_bytes(&mut self, pointer: u64, length: u64) -> FfiResult<Vec<u8>> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok(non_null(pointer).map(|at| {
            // SAFETY: as in `read`, for `length` bytes.
            unsafe { std::slice::from_raw_parts(at, length as usize) }.to_vec()
        }))
    }

    async fn write_bytes(&mut self, pointer: u64, length: u64, data: Vec<u8>) -> FfiResult<()> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok((|| {
            let at = non_null(pointer)?;
            if data.len() as u64 > length {
                return Err(failed(
                    "ERR_OUT_OF_RANGE",
                    format!("The value of \"len\" is out of range. It must be >= {}. Received {length}", data.len()),
                ));
            }
            // SAFETY: as in `write`, for `data.len()` bytes, which is at most `length`.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), at, data.len()) };
            Ok(())
        })())
    }

    async fn write_text(&mut self, pointer: u64, length: u64, data: String, encoding: String) -> FfiResult<()> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok((|| {
            let at = non_null(pointer)?;
            let bytes = encode_text(&data, &encoding)?;
            if bytes.len() as u64 > length {
                return Err(failed(
                    "ERR_OUT_OF_RANGE",
                    format!("The value of \"len\" is out of range. It must be >= {}. Received {length}", bytes.len()),
                ));
            }
            // SAFETY: as in `write_bytes`.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len()) };
            Ok(())
        })())
    }

    async fn current_event_loop(&mut self) -> FfiResult<u64> {
        if let Err(error) = self.ffi_allowed() {
            return Ok(Err(error));
        }
        Ok(Ok(std::ptr::addr_of!(EVENT_LOOP) as usize as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn define(name: &str, arguments: &[&str], returns: &str) -> Function {
        // SAFETY: the process image is always loadable.
        let handle = unsafe { libc::dlopen(std::ptr::null(), libc::RTLD_NOW) } as usize;
        Function {
            address: lookup(handle, name).unwrap(),
            arguments: arguments.iter().map(|name| Kind::parse(name).unwrap()).collect(),
            returns: Kind::parse(returns).unwrap(),
        }
    }

    #[test]
    fn calls_reach_libc_with_each_register_class() {
        let abs = define("abs", &["int32"], "int32");
        assert!(matches!(invoke(&abs, &[Value::Int32(-7)]).unwrap(), Value::Int32(7)));
        let fabs = define("fabs", &["double"], "double");
        assert!(matches!(invoke(&fabs, &[Value::Float64(-2.5)]).unwrap(), Value::Float64(v) if v == 2.5));
        let fabsf = define("fabsf", &["float"], "float");
        assert!(matches!(invoke(&fabsf, &[Value::Float32(-1.5)]).unwrap(), Value::Float32(v) if v == 1.5));
        let strlen = define("strlen", &["string"], "uint64");
        assert!(matches!(invoke(&strlen, &[Value::Text("hello".into())]).unwrap(), Value::Uint64(5)));
        // Mixed classes: ldexp(double, int).
        let ldexp = define("ldexp", &["double", "int32"], "double");
        assert!(matches!(invoke(&ldexp, &[Value::Float64(1.5), Value::Int32(3)]).unwrap(), Value::Float64(v) if v == 12.0));
    }

    #[test]
    fn strings_encode_as_node_exports_them() {
        assert_eq!(encode_text("hello", "utf8").unwrap(), b"hello\0");
        assert_eq!(encode_text("hi", "utf16le").unwrap(), [104, 0, 105, 0, 0, 0]);
        assert_eq!(encode_text("é", "latin1").unwrap(), [233, 0]);
    }
}
