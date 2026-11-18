//! `jco:node/inspector`: the host's view of a debugger that cannot exist here.
//!
//! Jco's Node provider drives the inspector of the V8 it runs on. This host executes the guest's
//! JavaScript inside a WebAssembly engine, out of reach of any inspector protocol backend, so it
//! answers as a Node process with no inspector attached would: nothing is listening, console and
//! network broadcasts go nowhere, and opening the inspector or connecting a session is refused as
//! unavailable rather than answered with invented protocol replies.

use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::inspector::{BroadcastTarget, CommandResponse, ConnectKind, Error, Failure, Host},
    hosts::denied_variant,
    plugin::NodeView,
    state::Capability,
};

type InspectorResult<T> = wasmtime::Result<Result<T, Error>>;

const UNAVAILABLE: &str =
    "the guest's JavaScript engine runs inside WebAssembly and exposes no inspector protocol to this host";

impl NodeView<'_> {
    fn inspector_allowed(&self) -> Result<(), Error> {
        if self.node.allows(Capability::Inspector) {
            Ok(())
        } else {
            Err(Error::Denied(denied_variant(Capability::Inspector)))
        }
    }
}

impl Host for NodeView<'_> {
    async fn open(&mut self, _port: Option<u32>, _host: Option<String>, _wait: bool) -> InspectorResult<()> {
        Ok(self.inspector_allowed().and(Err(Error::Unavailable(UNAVAILABLE.into()))))
    }

    async fn close(&mut self) -> InspectorResult<()> {
        // Closing an inspector that is not open is a no-op in Node.
        Ok(self.inspector_allowed())
    }

    async fn url(&mut self) -> InspectorResult<Option<String>> {
        Ok(self.inspector_allowed().map(|()| None))
    }

    async fn wait_for_debugger(&mut self) -> InspectorResult<()> {
        Ok(self.inspector_allowed().and(Err(Error::Failed(Failure {
            code: "ERR_INSPECTOR_NOT_ACTIVE".into(),
            message: "Inspector is not active".into(),
        }))))
    }

    async fn console_call(&mut self, _context: Option<String>, _method: String, _args_json: String) -> InspectorResult<()> {
        // `inspector.console` only reaches an attached frontend; with none, Node drops the call.
        Ok(self.inspector_allowed())
    }

    async fn session_connect(&mut self, _session: u32, _kind: ConnectKind, _listener: u32) -> InspectorResult<()> {
        Ok(self.inspector_allowed().and(Err(Error::Unavailable(UNAVAILABLE.into()))))
    }

    async fn session_post(
        &mut self,
        session: u32,
        _method: String,
        _params_json: Option<String>,
        _callback: Option<u32>,
    ) -> InspectorResult<Option<CommandResponse>> {
        // No session can have connected.
        Ok(self.inspector_allowed().and(Err(Error::NoSuchSession(session))))
    }

    async fn session_disconnect(&mut self, _session: u32) -> InspectorResult<()> {
        Ok(self.inspector_allowed())
    }

    async fn emit(&mut self, _target: BroadcastTarget, _event: String, _params_json: String) -> InspectorResult<()> {
        // `inspector.Network` broadcasts reach only connected frontends.
        Ok(self.inspector_allowed())
    }

    async fn put_network_resource(&mut self, _url: String, _data: String) -> InspectorResult<()> {
        Ok(self.inspector_allowed())
    }
}
