//! The host side of `jco:node@0.1.0`, the contract behind Jco's Node.js builtin support, as a
//! native wasmCloud host plugin.

pub mod callbacks;
pub mod errno;
pub mod hosts;
pub mod json;
pub mod plugin;
pub mod state;

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
            "jco:node/http.server": crate::hosts::http::HttpServer,
            "jco:node/zlib.engine": crate::hosts::zlib::ZlibEngine,
        },
    });
}
