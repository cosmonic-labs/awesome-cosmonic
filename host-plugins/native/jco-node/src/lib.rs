//! The host side of `jco:node@0.1.0`, the contract behind Jco's Node.js builtin support, as a
//! native wasmCloud host plugin.

pub mod callbacks;
pub mod errno;
pub mod hosts;
pub mod json;
pub mod plugin;
pub mod state;
pub mod wasi_tls;

pub use plugin::{JcoNode, PLUGIN_ID};
pub use state::{Capability, Grants, ProcessIdentity};

/// The Node.js release guests are told they run on unless the embedder says otherwise. Jco's
/// builtins follow this line.
pub const NODE_VERSION: &str = "v24.20.0";

pub mod bindings {
    wash_runtime::wasmtime::component::bindgen!({
        path: "wit",
        world: "imports",
        wasmtime_crate: wash_runtime::wasmtime,
        imports: { default: async | trappable },
        with: {
            "jco:node/http.client-request": crate::hosts::http::ClientRequest,
            "jco:node/http.connection": crate::hosts::http::HttpConnection,
            "jco:node/dgram.socket": crate::hosts::dgram::DgramSocket,
            "jco:node/http.server": crate::hosts::http::HttpServer,
            "jco:node/http2.client-session": crate::hosts::http2::ClientSession,
            "jco:node/http2.client-stream": crate::hosts::http2::ClientStream,
            "jco:node/http2.server": crate::hosts::http2::Http2Server,
            "jco:node/sqlite.cursor": crate::hosts::sqlite::SqliteCursor,
            "jco:node/sqlite.database": crate::hosts::sqlite::SqliteDatabase,
            "jco:node/sqlite.session": crate::hosts::sqlite::SqliteSession,
            "jco:node/sqlite.statement": crate::hosts::sqlite::SqliteStatement,
            "jco:node/sqlite.tag-store": crate::hosts::sqlite::SqliteTagStore,
            "jco:node/v8.profile": crate::hosts::v8::Profile,
            "jco:node/v8.reader": crate::hosts::v8::Reader,
            "jco:node/v8.writer": crate::hosts::v8::Writer,
            "jco:node/zlib.engine": crate::hosts::zlib::ZlibEngine,
            "wasi:io": wasmtime_wasi::p2::bindings::io,
            "wasi:tls/types": wasmtime_wasi_tls::p2::bindings::types,
        },
    });
}
