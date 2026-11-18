//! `jco:node/zlib`: Node's zlib, Brotli and Zstandard bindings, ported from `src/node_zlib.cc`
//! and the synchronous half of `lib/zlib.js` (v24).
//!
//! The guest validates option types; ranges, defaults, flush handling, multi-member gzip and
//! error shapes are Node's and live here, over a C-API zlib (`libz-rs-sys`), the `brotli` crate
//! and `libzstd`. Compressed bytes can differ from Node's, since the implementations differ;
//! what each decompressor accepts and how each failure reads do not.

use std::{
    ffi::{CStr, c_int, c_void},
    mem::size_of,
};

use brotli::{
    BrotliDecompressStream, BrotliResult, BrotliState,
    enc::{
        StandardAlloc,
        encode::{BrotliEncoderOperation, BrotliEncoderParameter, BrotliEncoderStateStruct},
    },
};
use libz_rs_sys as z;
use wash_runtime::wasmtime::{self, component::Resource};
use zstd_safe::zstd_sys as zstd;

use crate::{
    bindings::jco::node::zlib::{Algorithm, Errno, Error, Host, HostEngine, Options, Output},
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

type ZlibResult<T> = wasmtime::Result<Result<T, Error>>;

/// `buffer.constants.MAX_LENGTH` on 64-bit Node 24.
const MAX_LENGTH: f64 = 9_007_199_254_740_991.0;
const DEFAULT_CHUNK: f64 = 16_384.0;
const MIN_CHUNK: f64 = 64.0;

const Z_NO_FLUSH: i32 = 0;
const Z_FINISH: i32 = 4;
const Z_BLOCK: i32 = 5;
const BROTLI_PROCESS: i32 = 0;
const BROTLI_FINISH: i32 = 2;
const BROTLI_EMIT_METADATA: i32 = 3;
const ZSTD_CONTINUE: i32 = 0;
const ZSTD_END: i32 = 2;
/// The largest `ZSTD_c_*` and `ZSTD_d_*` parameter numbers Node accepts.
const MAX_ZSTD_C_PARAM: f64 = 402.0;
const MAX_ZSTD_D_PARAM: f64 = 100.0;
const MAX_BROTLI_PARAM: f64 = 9.0;

fn error(name: &str, code: &str, message: impl Into<String>) -> Error {
    Error { name: name.into(), message: message.into(), code: Some(code.into()), errno: None, syscall: None }
}

/// A failure of the compression library itself, which Node reports with its errno.
fn compression_error(message: impl Into<String>, code: impl Into<String>, errno: i64) -> Error {
    Error {
        name: "Error".into(),
        message: message.into(),
        code: Some(code.into()),
        errno: Some(Errno::Number(errno)),
        syscall: None,
    }
}

/// `String(number)` as JavaScript prints it, with Node's separators for large integers.
fn js_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value.fract() != 0.0 || value.abs() >= 1e21 {
        return value.to_string();
    }
    let digits = format!("{}", value.abs() as u128);
    let digits = if value.abs() > 4_294_967_296.0 {
        // `addNumericalSeparator` groups by threes from the right with underscores.
        let mut grouped = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index) % 3 == 0 {
                grouped.push('_');
            }
            grouped.push(digit);
        }
        grouped
    } else {
        digits
    };
    if value < 0.0 { format!("-{digits}") } else { digits }
}

fn out_of_range(name: &str, range: &str, received: f64) -> Error {
    error(
        "RangeError",
        "ERR_OUT_OF_RANGE",
        format!("The value of \"{name}\" is out of range. It must be {range}. Received {}", js_number(received)),
    )
}

/// `checkRangesOrGetDefault` from `lib/zlib.js`.
fn ranged(value: Option<f64>, name: &str, lower: f64, upper: f64, default: f64) -> Result<f64, Error> {
    let Some(value) = value else { return Ok(default) };
    if value.is_nan() {
        return Ok(default);
    }
    if value.is_infinite() {
        return Err(out_of_range(name, "a finite number", value));
    }
    if value < lower || value > upper {
        let upper = js_number(upper);
        return Err(out_of_range(name, &format!(">= {} and <= {upper}", js_number(lower)), value));
    }
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Deflate,
    Inflate,
    Gzip,
    Gunzip,
    DeflateRaw,
    InflateRaw,
    Unzip,
}

impl Mode {
    fn compresses(self) -> bool {
        matches!(self, Mode::Deflate | Mode::Gzip | Mode::DeflateRaw)
    }
}

enum Family {
    Zlib(Mode),
    Brotli { encode: bool },
    Zstd { compress: bool },
}

fn family(algorithm: Algorithm) -> Family {
    match algorithm {
        Algorithm::Deflate => Family::Zlib(Mode::Deflate),
        Algorithm::Inflate => Family::Zlib(Mode::Inflate),
        Algorithm::Gzip => Family::Zlib(Mode::Gzip),
        Algorithm::Gunzip => Family::Zlib(Mode::Gunzip),
        Algorithm::Deflateraw => Family::Zlib(Mode::DeflateRaw),
        Algorithm::Inflateraw => Family::Zlib(Mode::InflateRaw),
        Algorithm::Unzip => Family::Zlib(Mode::Unzip),
        Algorithm::Brotlicompress => Family::Brotli { encode: true },
        Algorithm::Brotlidecompress => Family::Brotli { encode: false },
        Algorithm::Zstdcompress => Family::Zstd { compress: true },
        Algorithm::Zstddecompress => Family::Zstd { compress: false },
    }
}

/// What `ZlibBase` keeps from the options, the same for every family.
#[derive(Clone)]
struct Settings {
    chunk: usize,
    flush: i32,
    finish: i32,
    max_output: f64,
    reject_garbage: bool,
}

fn settings(options: &Options, (lower, upper): (f64, f64), flush: i32, finish: i32) -> Result<Settings, Error> {
    let chunk = match options.chunk_size {
        Some(chunk) if chunk.is_finite() => {
            if chunk < MIN_CHUNK {
                return Err(out_of_range("options.chunkSize", ">= 64", chunk));
            }
            chunk
        }
        Some(chunk) if chunk.is_infinite() => return Err(out_of_range("options.chunkSize", "a finite number", chunk)),
        _ => DEFAULT_CHUNK,
    };
    Ok(Settings {
        chunk: chunk as usize,
        flush: ranged(options.flush, "options.flush", lower, upper, f64::from(flush))? as i32,
        finish: ranged(options.finish_flush, "options.finishFlush", lower, upper, f64::from(finish))? as i32,
        max_output: ranged(options.max_output_length, "options.maxOutputLength", 1.0, MAX_LENGTH, MAX_LENGTH)?,
        reject_garbage: options.reject_garbage_after_end == Some(true),
    })
}

/// How far one call into a codec got.
struct Step {
    consumed: usize,
    produced: usize,
}

// ---- zlib -------------------------------------------------------------------------------------

struct ZlibCodec {
    mode: Mode,
    /// zlib keeps a pointer back to its stream, so the stream never moves.
    stream: Box<z::z_stream>,
    dictionary: Vec<u8>,
    level: i32,
    strategy: i32,
    /// For `unzip`: how much of the gzip magic has been seen.
    gzip_id_bytes_read: u8,
    err: i32,
}

// SAFETY: the stream's pointers refer only to buffers the codec is handed for one call, and to
// zlib state that the codec owns and touches from one thread at a time.
unsafe impl Send for ZlibCodec {}

impl Drop for ZlibCodec {
    fn drop(&mut self) {
        // SAFETY: the stream was initialised for this direction and is ended once.
        unsafe {
            if self.mode.compresses() {
                z::deflateEnd(&mut *self.stream);
            } else {
                z::inflateEnd(&mut *self.stream);
            }
        }
    }
}

fn zlib_code(err: i32) -> &'static str {
    match err {
        z::Z_OK => "Z_OK",
        z::Z_STREAM_END => "Z_STREAM_END",
        z::Z_NEED_DICT => "Z_NEED_DICT",
        z::Z_ERRNO => "Z_ERRNO",
        z::Z_STREAM_ERROR => "Z_STREAM_ERROR",
        z::Z_DATA_ERROR => "Z_DATA_ERROR",
        z::Z_MEM_ERROR => "Z_MEM_ERROR",
        z::Z_BUF_ERROR => "Z_BUF_ERROR",
        z::Z_VERSION_ERROR => "Z_VERSION_ERROR",
        _ => "Z_UNKNOWN_ERROR",
    }
}

fn initialization_failed() -> Error {
    error("Error", "ERR_ZLIB_INITIALIZATION_FAILED", "Initialization failed")
}

impl ZlibCodec {
    fn open(mode: Mode, options: &Options) -> Result<Self, Error> {
        let inflating = matches!(mode, Mode::Inflate | Mode::Gunzip | Mode::Unzip);
        let mut window_bits = if inflating && matches!(options.window_bits, None | Some(0.0)) {
            0
        } else {
            let lower = 8.0 + if mode == Mode::Gzip { 1.0 } else { 0.0 };
            ranged(options.window_bits, "options.windowBits", lower, 15.0, 15.0)? as i32
        };
        let level = ranged(options.level, "options.level", -1.0, 9.0, -1.0)? as i32;
        let mem_level = ranged(options.mem_level, "options.memLevel", 1.0, 9.0, 8.0)? as i32;
        let strategy = ranged(options.strategy, "options.strategy", 0.0, 4.0, 0.0)? as i32;
        if mode == Mode::DeflateRaw && window_bits == 8 {
            // zlib cannot write raw streams with a 256-byte window; Node quietly uses 512.
            window_bits = 9;
        }
        match mode {
            Mode::Gzip | Mode::Gunzip => window_bits += 16,
            Mode::Unzip => window_bits += 32,
            Mode::DeflateRaw | Mode::InflateRaw => window_bits = -window_bits,
            _ => {}
        }
        let mut codec = ZlibCodec {
            mode,
            stream: Box::default(),
            dictionary: options.dictionary.clone().unwrap_or_default(),
            level,
            strategy,
            gzip_id_bytes_read: 0,
            err: z::Z_OK,
        };
        let version = z::zlibVersion();
        let size = size_of::<z::z_stream>() as c_int;
        // SAFETY: the stream is a zeroed, boxed `z_stream`, as the init functions expect.
        let status = unsafe {
            if mode.compresses() {
                z::deflateInit2_(&mut *codec.stream, level, z::Z_DEFLATED, window_bits, mem_level, strategy, version, size)
            } else {
                z::inflateInit2_(&mut *codec.stream, window_bits, version, size)
            }
        };
        if status != z::Z_OK {
            // Nothing to end: forget the stream instead of dropping it.
            std::mem::forget(codec);
            return Err(initialization_failed());
        }
        codec.set_dictionary().map_err(|_| initialization_failed())?;
        Ok(codec)
    }

    fn set_dictionary(&mut self) -> Result<(), Error> {
        if self.dictionary.is_empty() {
            return Ok(());
        }
        let length = self.dictionary.len() as u32;
        // SAFETY: the dictionary outlives the call, which copies it.
        self.err = unsafe {
            match self.mode {
                Mode::Deflate | Mode::DeflateRaw => {
                    z::deflateSetDictionary(&mut *self.stream, self.dictionary.as_ptr(), length)
                }
                Mode::InflateRaw => z::inflateSetDictionary(&mut *self.stream, self.dictionary.as_ptr(), length),
                _ => z::Z_OK,
            }
        };
        if self.err != z::Z_OK {
            return Err(self.error_for("Failed to set dictionary"));
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), Error> {
        self.err = z::Z_OK;
        // SAFETY: the stream is initialised for this direction.
        self.err = unsafe {
            if self.mode.compresses() { z::deflateReset(&mut *self.stream) } else { z::inflateReset(&mut *self.stream) }
        };
        if self.err != z::Z_OK {
            return Err(self.error_for("Failed to reset stream"));
        }
        self.set_dictionary()
    }

    fn error_for(&self, message: &str) -> Error {
        let message = if self.stream.msg.is_null() {
            message.to_owned()
        } else {
            // SAFETY: zlib's messages are static NUL-terminated strings.
            unsafe { CStr::from_ptr(self.stream.msg) }.to_string_lossy().into_owned()
        };
        compression_error(message, zlib_code(self.err), i64::from(self.err))
    }

    /// `ZlibContext::DoThreadPoolWork` followed by `GetErrorInfo`.
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Result<Step, Error> {
        let stream = &mut *self.stream;
        stream.next_in = input.as_ptr();
        stream.avail_in = input.len() as u32;
        stream.next_out = output.as_mut_ptr();
        stream.avail_out = output.len() as u32;
        // SAFETY: the stream's buffers are `input` and `output`, both live for this call.
        unsafe {
            if self.mode.compresses() {
                self.err = z::deflate(stream, flush);
            } else {
                if self.mode == Mode::Unzip && stream.avail_in > 0 {
                    // Sniff the gzip magic so a gzip stream gets gunzip's member handling.
                    let mut next = stream.next_in;
                    let mut available = stream.avail_in;
                    if self.gzip_id_bytes_read == 0 {
                        if *next == 0x1f {
                            self.gzip_id_bytes_read = 1;
                            next = next.add(1);
                            available -= 1;
                        } else {
                            self.mode = Mode::Inflate;
                        }
                    }
                    if self.mode == Mode::Unzip && self.gzip_id_bytes_read == 1 && available > 0 {
                        if *next == 0x8b {
                            self.gzip_id_bytes_read = 2;
                            self.mode = Mode::Gunzip;
                        } else {
                            self.mode = Mode::Inflate;
                        }
                    }
                }
                self.err = z::inflate(stream, flush);
                if self.mode != Mode::InflateRaw && self.err == z::Z_NEED_DICT && !self.dictionary.is_empty() {
                    self.err =
                        z::inflateSetDictionary(stream, self.dictionary.as_ptr(), self.dictionary.len() as u32);
                    if self.err == z::Z_OK {
                        self.err = z::inflate(stream, flush);
                    } else if self.err == z::Z_DATA_ERROR {
                        // A dictionary that does not match the stream is reported as needing one.
                        self.err = z::Z_NEED_DICT;
                    }
                }
                // Another gzip member may follow; a zero byte after a member ends the input.
                while stream.avail_in > 0 && self.mode == Mode::Gunzip && self.err == z::Z_STREAM_END && *stream.next_in != 0
                {
                    self.err = z::inflateReset(stream);
                    if self.err == z::Z_OK {
                        self.err = z::inflate(stream, flush);
                    }
                }
            }
        }
        let step = Step {
            consumed: input.len() - stream.avail_in as usize,
            produced: output.len() - stream.avail_out as usize,
        };
        let avail_out = stream.avail_out;
        match self.err {
            z::Z_OK | z::Z_BUF_ERROR if avail_out != 0 && flush == Z_FINISH => {
                Err(self.error_for("unexpected end of file"))
            }
            z::Z_OK | z::Z_BUF_ERROR | z::Z_STREAM_END => Ok(step),
            z::Z_NEED_DICT if self.dictionary.is_empty() => Err(self.error_for("Missing dictionary")),
            z::Z_NEED_DICT => Err(self.error_for("Bad dictionary")),
            _ => Err(self.error_for("Zlib error")),
        }
    }

    fn params(&mut self, level: i32, strategy: i32, output: &mut [u8]) -> Result<usize, Error> {
        if !matches!(self.mode, Mode::Deflate | Mode::DeflateRaw) {
            return Ok(0);
        }
        let stream = &mut *self.stream;
        stream.next_in = std::ptr::null();
        stream.avail_in = 0;
        stream.next_out = output.as_mut_ptr();
        stream.avail_out = output.len() as u32;
        // SAFETY: the output buffer is live for this call; there is no input.
        self.err = unsafe { z::deflateParams(stream, level, strategy) };
        let produced = output.len() - stream.avail_out as usize;
        if self.err != z::Z_OK && self.err != z::Z_BUF_ERROR {
            return Err(self.error_for("Failed to set parameters"));
        }
        self.level = level;
        self.strategy = strategy;
        Ok(produced)
    }
}

// ---- Brotli -----------------------------------------------------------------------------------

type Decoder = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;

enum BrotliCodec {
    Encoder(Box<BrotliEncoderStateStruct<StandardAlloc>>),
    Decoder(Box<Decoder>),
}

fn encoder_parameter(key: u32) -> Option<BrotliEncoderParameter> {
    use BrotliEncoderParameter::*;
    Some(match key {
        0 => BROTLI_PARAM_MODE,
        1 => BROTLI_PARAM_QUALITY,
        2 => BROTLI_PARAM_LGWIN,
        3 => BROTLI_PARAM_LGBLOCK,
        4 => BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING,
        5 => BROTLI_PARAM_SIZE_HINT,
        6 => BROTLI_PARAM_LARGE_WINDOW,
        // NPOSTFIX, NDIRECT and STREAM_OFFSET have no setter in the Rust encoder.
        _ => return None,
    })
}

impl BrotliCodec {
    fn open(encode: bool, params: &[(u32, u32)]) -> Result<Self, Error> {
        if encode {
            let mut state = Box::new(BrotliEncoderStateStruct::new(StandardAlloc::default()));
            for (key, value) in params {
                let accepted = encoder_parameter(*key).is_some_and(|parameter| state.set_parameter(parameter, *value));
                if !accepted {
                    return Err(initialization_failed());
                }
            }
            return Ok(BrotliCodec::Encoder(state));
        }
        let mut state =
            Box::new(Decoder::new_strict(StandardAlloc::default(), StandardAlloc::default(), StandardAlloc::default()));
        for (key, value) in params {
            match key {
                // DISABLE_RING_BUFFER_REALLOCATION only changes how memory grows.
                0 => {}
                1 => state.large_window = *value != 0,
                _ => return Err(initialization_failed()),
            }
        }
        Ok(BrotliCodec::Decoder(state))
    }

    fn step(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Result<Step, Error> {
        let mut available_in = input.len();
        let mut input_offset = 0;
        let mut available_out = output.len();
        let mut output_offset = 0;
        match self {
            BrotliCodec::Encoder(state) => {
                let operation = match flush {
                    0 => BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
                    1 => BrotliEncoderOperation::BROTLI_OPERATION_FLUSH,
                    2 => BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
                    _ => BrotliEncoderOperation::BROTLI_OPERATION_EMIT_METADATA,
                };
                let mut total = None;
                let ok = state.compress_stream(
                    operation,
                    &mut available_in,
                    input,
                    &mut input_offset,
                    &mut available_out,
                    output,
                    &mut output_offset,
                    &mut total,
                    &mut |_, _, _, _| (),
                );
                if !ok {
                    return Err(compression_error("Compression failed", "ERR_BROTLI_COMPRESSION_FAILED", -1));
                }
            }
            BrotliCodec::Decoder(state) => {
                let mut total = 0;
                let result = BrotliDecompressStream(
                    &mut available_in,
                    &mut input_offset,
                    input,
                    &mut available_out,
                    &mut output_offset,
                    output,
                    &mut total,
                    state,
                );
                match result {
                    BrotliResult::ResultFailure => {
                        let code = state.error_code;
                        // Node prefixes libbrotli's `BrotliDecoderErrorString`, which drops
                        // the `BROTLI_DECODER` part of the name.
                        let name = format!("{code:?}");
                        let name = name.strip_prefix("BROTLI_DECODER").unwrap_or(&name);
                        return Err(compression_error("Decompression failed", format!("ERR_{name}"), code as i64));
                    }
                    BrotliResult::NeedsMoreInput if flush == BROTLI_FINISH => {
                        return Err(compression_error(
                            "unexpected end of file",
                            "Z_BUF_ERROR",
                            i64::from(z::Z_BUF_ERROR),
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(Step { consumed: input_offset, produced: output_offset })
    }
}

// ---- Zstandard --------------------------------------------------------------------------------

// zstd-sys declares the parameter arguments as Rust enums, and an arbitrary number is not a valid
// value of one. These declarations take the same symbols with the C signature.
#[allow(clashing_extern_declarations)]
unsafe extern "C" {
    #[link_name = "ZSTD_CCtx_setParameter"]
    fn zstd_cctx_set_parameter(cctx: *mut zstd::ZSTD_CCtx, param: c_int, value: c_int) -> usize;
    #[link_name = "ZSTD_DCtx_setParameter"]
    fn zstd_dctx_set_parameter(dctx: *mut zstd::ZSTD_DCtx, param: c_int, value: c_int) -> usize;
}

enum ZstdCodec {
    Compress(*mut zstd::ZSTD_CCtx),
    Decompress(*mut zstd::ZSTD_DCtx),
}

// SAFETY: the context is owned by the codec and used from one thread at a time.
unsafe impl Send for ZstdCodec {}

impl Drop for ZstdCodec {
    fn drop(&mut self) {
        // SAFETY: each context was created by the matching constructor and is freed once.
        unsafe {
            match *self {
                ZstdCodec::Compress(context) => drop(zstd::ZSTD_freeCCtx(context)),
                ZstdCodec::Decompress(context) => drop(zstd::ZSTD_freeDCtx(context)),
            }
        }
    }
}

fn zstd_error(code: usize) -> Error {
    // SAFETY: both calls only classify the result code; the name is a static string.
    let (kind, message) = unsafe {
        (zstd::ZSTD_getErrorCode(code), CStr::from_ptr(zstd::ZSTD_getErrorName(code)).to_string_lossy().into_owned())
    };
    compression_error(message, format!("{kind:?}"), kind as i64)
}

fn zstd_failed(result: usize) -> bool {
    // SAFETY: `ZSTD_isError` only inspects the value.
    unsafe { zstd::ZSTD_isError(result) != 0 }
}

impl ZstdCodec {
    fn open(compress: bool, params: &[(u32, u32)], pledged: Option<f64>, dictionary: &[u8]) -> Result<Self, Error> {
        let failed = |_| error("Error", "ERR_ZLIB_INITIALIZATION_FAILED", "Setting parameter failed");
        // SAFETY: the contexts are created here, owned by the codec, and freed on drop.
        unsafe {
            if compress {
                let codec = ZstdCodec::Compress(zstd::ZSTD_createCCtx());
                let ZstdCodec::Compress(context) = codec else { unreachable!() };
                for (key, value) in params {
                    let result = zstd_cctx_set_parameter(context, *key as c_int, *value as c_int);
                    if zstd_failed(result) {
                        return Err(failed(result));
                    }
                }
                if let Some(pledged) = pledged {
                    let result = zstd::ZSTD_CCtx_setPledgedSrcSize(context, pledged as u64);
                    if zstd_failed(result) {
                        return Err(error("Error", "ERR_ZLIB_INITIALIZATION_FAILED", "Setting pledged size failed"));
                    }
                }
                if !dictionary.is_empty() {
                    let result =
                        zstd::ZSTD_CCtx_loadDictionary(context, dictionary.as_ptr().cast(), dictionary.len());
                    if zstd_failed(result) {
                        return Err(error("Error", "ERR_ZLIB_INITIALIZATION_FAILED", "Failed to load dictionary"));
                    }
                }
                Ok(codec)
            } else {
                let codec = ZstdCodec::Decompress(zstd::ZSTD_createDCtx());
                let ZstdCodec::Decompress(context) = codec else { unreachable!() };
                for (key, value) in params {
                    let result = zstd_dctx_set_parameter(context, *key as c_int, *value as c_int);
                    if zstd_failed(result) {
                        return Err(failed(result));
                    }
                }
                if !dictionary.is_empty() {
                    let result =
                        zstd::ZSTD_DCtx_loadDictionary(context, dictionary.as_ptr().cast(), dictionary.len());
                    if zstd_failed(result) {
                        return Err(error("Error", "ERR_ZLIB_INITIALIZATION_FAILED", "Failed to load dictionary"));
                    }
                }
                Ok(codec)
            }
        }
    }

    fn step(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Result<Step, Error> {
        let mut source = zstd::ZSTD_inBuffer { src: input.as_ptr().cast::<c_void>(), size: input.len(), pos: 0 };
        let mut destination =
            zstd::ZSTD_outBuffer { dst: output.as_mut_ptr().cast::<c_void>(), size: output.len(), pos: 0 };
        // SAFETY: the buffers describe `input` and `output`, both live for this call.
        let result = unsafe {
            match *self {
                ZstdCodec::Compress(context) => {
                    let directive = match flush {
                        0 => zstd::ZSTD_EndDirective::ZSTD_e_continue,
                        1 => zstd::ZSTD_EndDirective::ZSTD_e_flush,
                        _ => zstd::ZSTD_EndDirective::ZSTD_e_end,
                    };
                    zstd::ZSTD_compressStream2(context, &mut destination, &mut source, directive)
                }
                ZstdCodec::Decompress(context) => zstd::ZSTD_decompressStream(context, &mut destination, &mut source),
            }
        };
        if zstd_failed(result) {
            return Err(zstd_error(result));
        }
        Ok(Step { consumed: source.pos, produced: destination.pos })
    }

    fn reset(&mut self) {
        // SAFETY: resetting the session keeps parameters and dictionary, as Node's reset does.
        unsafe {
            match *self {
                ZstdCodec::Compress(context) => {
                    zstd::ZSTD_CCtx_reset(context, zstd::ZSTD_ResetDirective::ZSTD_reset_session_only);
                }
                ZstdCodec::Decompress(context) => {
                    zstd::ZSTD_DCtx_reset(context, zstd::ZSTD_ResetDirective::ZSTD_reset_session_only);
                }
            }
        }
    }
}

// ---- One engine for all three ------------------------------------------------------------------

enum Codec {
    Zlib(ZlibCodec),
    Brotli(BrotliCodec),
    Zstd(ZstdCodec),
}

/// One native stream: the codec, Node's settings for it, and what the guest has written.
pub struct ZlibEngine {
    codec: Option<Codec>,
    settings: Settings,
    /// How the codec was opened, to open it again on `reset` where the library cannot.
    algorithm: Algorithm,
    options: Options,
    bytes_written: f64,
}

/// Numeric parameter keys, rejected the way `lib/zlib.js` rejects them.
fn parameters(options: &Options, max: f64, code: &str, family: &str) -> Result<Vec<(u32, u32)>, Error> {
    let mut seen = Vec::new();
    for (key, value) in options.params.iter().flatten() {
        let number: f64 = key.trim().parse().unwrap_or(f64::NAN);
        let duplicate = seen.iter().any(|(seen, _)| f64::from(*seen) == number);
        if key.trim().is_empty() || number.is_nan() || number < 0.0 || number > max || duplicate {
            return Err(error("RangeError", code, format!("{key} is not a valid {family} parameter")));
        }
        // The native side stores each value in a `uint32_t`.
        seen.push((number as u32, *value as i64 as u32));
    }
    Ok(seen)
}

fn open(algorithm: Algorithm, options: &Options) -> Result<(Codec, Settings), Error> {
    match family(algorithm) {
        Family::Zlib(mode) => {
            let settings = settings(options, (f64::from(Z_NO_FLUSH), f64::from(Z_BLOCK)), Z_NO_FLUSH, Z_FINISH)?;
            Ok((Codec::Zlib(ZlibCodec::open(mode, options)?), settings))
        }
        Family::Brotli { encode } => {
            let settings = settings(
                options,
                (f64::from(BROTLI_PROCESS), f64::from(BROTLI_EMIT_METADATA)),
                BROTLI_PROCESS,
                BROTLI_FINISH,
            )?;
            let params = parameters(options, MAX_BROTLI_PARAM, "ERR_BROTLI_INVALID_PARAM", "Brotli")?;
            Ok((Codec::Brotli(BrotliCodec::open(encode, &params)?), settings))
        }
        Family::Zstd { compress } => {
            let settings =
                settings(options, (f64::from(ZSTD_CONTINUE), f64::from(ZSTD_END)), ZSTD_CONTINUE, ZSTD_END)?;
            let max = if compress { MAX_ZSTD_C_PARAM } else { MAX_ZSTD_D_PARAM };
            let params = parameters(options, max, "ERR_ZSTD_INVALID_PARAM", "zstd")?;
            let dictionary = options.dictionary.as_deref().unwrap_or_default();
            Ok((Codec::Zstd(ZstdCodec::open(compress, &params, options.pledged_src_size, dictionary)?), settings))
        }
    }
}

impl Codec {
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: i32) -> Result<Step, Error> {
        match self {
            Codec::Zlib(codec) => codec.step(input, output, flush),
            Codec::Brotli(codec) => codec.step(input, output, flush),
            Codec::Zstd(codec) => codec.step(input, output, flush),
        }
    }

    /// `processChunkSync`: call the codec until it leaves output space unused.
    fn process(
        &mut self,
        input: &[u8],
        flush: i32,
        settings: &Settings,
        limit: Option<f64>,
    ) -> Result<(Vec<u8>, usize), Error> {
        let mut offset = 0;
        let mut data = Vec::new();
        let mut buffer = vec![0u8; settings.chunk];
        loop {
            let step = self.step(&input[offset..], &mut buffer, flush)?;
            offset += step.consumed;
            data.extend_from_slice(&buffer[..step.produced]);
            if let Some(limit) = limit
                && data.len() as f64 > limit
            {
                return Err(error(
                    "RangeError",
                    "ERR_BUFFER_TOO_LARGE",
                    format!("Cannot create a Buffer larger than {} bytes", js_number(limit)),
                ));
            }
            if step.produced < buffer.len() {
                break;
            }
        }
        if settings.reject_garbage && offset < input.len() {
            return Err(error(
                "Error",
                "ERR_TRAILING_JUNK_AFTER_STREAM_END",
                "Trailing junk found after the end of the compressed stream",
            ));
        }
        Ok((data, offset))
    }
}

fn closed() -> Error {
    error("Error", "ERR_ASSERTION", "zlib binding closed")
}

impl ZlibEngine {
    fn codec(&mut self) -> Result<&mut Codec, Error> {
        self.codec.as_mut().ok_or_else(closed)
    }

    fn run(&mut self, input: &[u8], flush: i32) -> Result<Output, Error> {
        let settings = self.settings.clone();
        let (data, consumed) = self.codec()?.process(input, flush, &settings, None)?;
        self.bytes_written += consumed as f64;
        Ok(Output { data, bytes_written: self.bytes_written })
    }
}

impl NodeView<'_> {
    fn zlib_granted(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Zlib) {
            return Ok(());
        }
        Err(error("Error", &Capability::Zlib.denied_code(), denied_message(Capability::Zlib)))
    }
}

macro_rules! granted {
    ($view:expr) => {
        if let Err(error) = $view.zlib_granted() {
            return Ok(Err(error));
        }
    };
}

macro_rules! engine {
    ($view:expr, $self_:expr) => {
        match $view.table.get_mut(&$self_) {
            Ok(engine) => engine,
            Err(_) => return Ok(Err(closed())),
        }
    };
}

impl HostEngine for NodeView<'_> {
    async fn write(&mut self, self_: Resource<ZlibEngine>, data: Vec<u8>) -> ZlibResult<Output> {
        granted!(self);
        let engine = engine!(self, self_);
        let flush = engine.settings.flush;
        Ok(engine.run(&data, flush))
    }

    async fn finish(&mut self, self_: Resource<ZlibEngine>) -> ZlibResult<Output> {
        granted!(self);
        let engine = engine!(self, self_);
        let finish = engine.settings.finish;
        Ok(engine.run(&[], finish))
    }

    async fn flush(&mut self, self_: Resource<ZlibEngine>, kind: f64) -> ZlibResult<Output> {
        granted!(self);
        let engine = engine!(self, self_);
        Ok(engine.run(&[], kind as i32))
    }

    async fn params(&mut self, self_: Resource<ZlibEngine>, level: f64, strategy: f64) -> ZlibResult<Output> {
        granted!(self);
        let engine = engine!(self, self_);
        let chunk = engine.settings.chunk;
        let written = engine.bytes_written;
        Ok((|| {
            let Codec::Zlib(codec) = engine.codec()? else {
                return Err(error("TypeError", "ERR_INVALID_ARG_TYPE", "params is only supported by zlib streams"));
            };
            let mut buffer = vec![0u8; chunk];
            let produced = codec.params(level as i32, strategy as i32, &mut buffer)?;
            buffer.truncate(produced);
            Ok(Output { data: buffer, bytes_written: written })
        })())
    }

    async fn reset(&mut self, self_: Resource<ZlibEngine>) -> ZlibResult<()> {
        granted!(self);
        let engine = engine!(self, self_);
        Ok((|| match engine.codec()? {
            Codec::Zlib(codec) => {
                codec.gzip_id_bytes_read = 0;
                codec.reset()
            }
            Codec::Zstd(codec) => {
                codec.reset();
                Ok(())
            }
            Codec::Brotli(_) => {
                // libbrotli has no reset; Node makes a new state, as this does.
                let (codec, _) = open(engine.algorithm, &engine.options)?;
                engine.codec = Some(codec);
                Ok(())
            }
        })())
    }

    async fn close(&mut self, self_: Resource<ZlibEngine>) -> ZlibResult<()> {
        granted!(self);
        let engine = engine!(self, self_);
        engine.codec = None;
        Ok(Ok(()))
    }

    async fn drop(&mut self, rep: Resource<ZlibEngine>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

impl Host for NodeView<'_> {
    async fn compress(&mut self, algorithm: Algorithm, data: Vec<u8>, options: Options) -> ZlibResult<Output> {
        granted!(self);
        // Compression is CPU-bound, so it runs off the async workers.
        Ok(tokio::task::spawn_blocking(move || {
            let (mut codec, settings) = open(algorithm, &options)?;
            let (data, consumed) =
                codec.process(&data, settings.finish, &settings, Some(settings.max_output))?;
            Ok(Output { data, bytes_written: consumed as f64 })
        })
        .await?)
    }

    async fn crc32(&mut self, data: Vec<u8>, value: u32) -> ZlibResult<u32> {
        granted!(self);
        let mut crc = std::ffi::c_ulong::from(value);
        for chunk in data.chunks(u32::MAX as usize) {
            // SAFETY: the pointer and length describe `chunk`.
            crc = unsafe { z::crc32(crc, chunk.as_ptr(), chunk.len() as u32) };
        }
        Ok(Ok(crc as u32))
    }

    async fn open(&mut self, algorithm: Algorithm, options: Options) -> ZlibResult<Resource<ZlibEngine>> {
        granted!(self);
        let (codec, settings) = match open(algorithm, &options) {
            Ok(opened) => opened,
            Err(error) => return Ok(Err(error)),
        };
        let engine = ZlibEngine { codec: Some(codec), settings, algorithm, options, bytes_written: 0.0 };
        Ok(Ok(self.table.push(engine)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options {
        Options {
            flush: None,
            finish_flush: None,
            chunk_size: None,
            window_bits: None,
            level: None,
            mem_level: None,
            strategy: None,
            max_output_length: None,
            pledged_src_size: None,
            reject_garbage_after_end: None,
            dictionary: None,
            params: None,
        }
    }

    fn compress(algorithm: Algorithm, data: &[u8], options: &Options) -> Result<Vec<u8>, Error> {
        let (mut codec, settings) = open(algorithm, options)?;
        codec.process(data, settings.finish, &settings, Some(settings.max_output)).map(|(data, _)| data)
    }

    #[test]
    fn every_codec_round_trips() {
        let input = "component zlib 💚".repeat(20).into_bytes();
        for (encode, decode) in [
            (Algorithm::Gzip, Algorithm::Gunzip),
            (Algorithm::Deflate, Algorithm::Inflate),
            (Algorithm::Deflateraw, Algorithm::Inflateraw),
            (Algorithm::Gzip, Algorithm::Unzip),
            (Algorithm::Deflate, Algorithm::Unzip),
            (Algorithm::Brotlicompress, Algorithm::Brotlidecompress),
            (Algorithm::Zstdcompress, Algorithm::Zstddecompress),
        ] {
            let packed = compress(encode, &input, &options()).unwrap();
            assert_eq!(compress(decode, &packed, &options()).unwrap(), input, "{encode:?} -> {decode:?}");
        }
    }

    // The expectations below are what Node 24 reports for the same calls.
    #[test]
    fn failures_read_like_node() {
        let corrupt = compress(Algorithm::Gunzip, b"invalid", &options()).unwrap_err();
        assert_eq!((corrupt.code.as_deref(), corrupt.message.as_str()), (Some("Z_DATA_ERROR"), "incorrect header check"));
        assert!(matches!(corrupt.errno, Some(Errno::Number(-3))));

        let packed = compress(Algorithm::Deflate, b"abc", &options()).unwrap();
        let truncated = compress(Algorithm::Inflate, &packed[..5], &options()).unwrap_err();
        assert_eq!((truncated.code.as_deref(), truncated.message.as_str()), (Some("Z_BUF_ERROR"), "unexpected end of file"));

        let brotli = compress(Algorithm::Brotlidecompress, b"garbage", &options()).unwrap_err();
        assert_eq!(brotli.message, "Decompression failed");
        assert!(brotli.code.unwrap().starts_with("ERR__ERROR_FORMAT"));

        let zstd = compress(Algorithm::Zstddecompress, b"garbage", &options()).unwrap_err();
        assert_eq!((zstd.code.as_deref(), zstd.message.as_str()), (Some("ZSTD_error_prefix_unknown"), "Unknown frame descriptor"));
        assert!(matches!(zstd.errno, Some(Errno::Number(10))));

        let limited = Options { max_output_length: Some(5.0), ..options() };
        let large = compress(Algorithm::Gzip, &[b'a'; 1000], &limited).unwrap_err();
        assert_eq!(large.message, "Cannot create a Buffer larger than 5 bytes");

        let window = Options { window_bits: Some(3.0), ..options() };
        assert_eq!(
            compress(Algorithm::Deflate, b"a", &window).unwrap_err().message,
            "The value of \"options.windowBits\" is out of range. It must be >= 8 and <= 15. Received 3"
        );
    }

    #[test]
    fn dictionaries_follow_node() {
        let with = Options { dictionary: Some(b"abc".to_vec()), ..options() };
        let packed = compress(Algorithm::Deflate, b"abc", &with).unwrap();
        assert_eq!(compress(Algorithm::Inflate, &packed, &with).unwrap(), b"abc");
        assert_eq!(compress(Algorithm::Inflate, &packed, &options()).unwrap_err().message, "Missing dictionary");
        let wrong = Options { dictionary: Some(b"xyz".to_vec()), ..options() };
        assert_eq!(compress(Algorithm::Inflate, &packed, &wrong).unwrap_err().message, "Bad dictionary");
    }

    #[test]
    fn gzip_members_and_trailing_bytes_follow_node() {
        let a = compress(Algorithm::Gzip, b"a", &options()).unwrap();
        let b = compress(Algorithm::Gzip, b"b", &options()).unwrap();
        assert_eq!(compress(Algorithm::Gunzip, &[a.clone(), b].concat(), &options()).unwrap(), b"ab");
        assert_eq!(compress(Algorithm::Gunzip, &[a.clone(), vec![0, 0x6a]].concat(), &options()).unwrap(), b"a");
        let junk = compress(Algorithm::Gunzip, &[a.clone(), b"ju".to_vec()].concat(), &options()).unwrap_err();
        assert_eq!(junk.code.as_deref(), Some("Z_DATA_ERROR"));
        let strict = Options { reject_garbage_after_end: Some(true), ..options() };
        let rejected = compress(Algorithm::Gunzip, &[a, vec![0]].concat(), &strict).unwrap_err();
        assert_eq!(rejected.code.as_deref(), Some("ERR_TRAILING_JUNK_AFTER_STREAM_END"));
    }

    #[test]
    fn numbers_print_like_javascript() {
        assert_eq!(js_number(3.0), "3");
        assert_eq!(js_number(1.5), "1.5");
        assert_eq!(js_number(9_007_199_254_740_991.0), "9_007_199_254_740_991");
    }
}
