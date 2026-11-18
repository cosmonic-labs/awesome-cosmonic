//! The wasmCloud side: binding `jco:node` into a workload's linker.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use wash_runtime::{
    engine::{ctx::SharedCtx, workload::WorkloadItem},
    plugin::{HostPlugin, WitInterfaces},
    wasmtime::component::{HasData, Linker, ResourceTable},
    wit::{WitInterface, WitWorld},
};

use crate::{
    bindings::jco::node,
    state::{Capability, Grants, InstanceTemplate, NodeInstance, ProcessIdentity, lock},
};

/// The id the plugin registers under and is looked up by.
pub const PLUGIN_ID: &str = "jco-node";

/// The WIT package the plugin serves.
pub const PACKAGE_VERSION: &str = "0.1.0";

type Instances = Mutex<HashMap<Arc<str>, Arc<NodeInstance>>>;

/// The host side of `jco:node@0.1.0` as a native host plugin.
///
/// A workload names the interfaces it wants in its `host_interfaces`, for example
/// `jco:node/console,fs,process@0.1.0`, and those are what its components are granted. Every
/// other `jco:node` interface is still linked, so a component that imports one instantiates, but
/// each call refuses with the error Jco's own default provider raises.
#[derive(Default)]
pub struct JcoNode {
    identity: ProcessIdentity,
    /// What to build a component's instances from, by component context id.
    templates: Mutex<HashMap<String, Arc<InstanceTemplate>>>,
    /// Which components each workload bound, so unbinding can forget them.
    workloads: Mutex<HashMap<String, Vec<String>>>,
    /// Live instances by store context id. An entry lasts as long as its store: the store's
    /// resource table holds a guard that removes it.
    instances: Arc<Instances>,
}

impl JcoNode {
    pub fn new() -> Self {
        Self::default()
    }

    /// Describe the process guests see: `argv`, `execPath`, starting directory, Node version.
    pub fn with_identity(mut self, identity: ProcessIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// The state of the instance running in the store context `id`, if it has made a call yet.
    pub fn instance(&self, id: &str) -> Option<Arc<NodeInstance>> {
        lock(&self.instances).get(id).cloned()
    }

    /// The state of the instance running in `shared`'s store, created on first use.
    pub fn attach(&self, shared: &mut SharedCtx) -> Arc<NodeInstance> {
        let id = shared.active_ctx.id.clone();
        if let Some(instance) = lock(&self.instances).get(&id) {
            return instance.clone();
        }
        let template = lock(&self.templates)
            .get(&*shared.active_ctx.component_id)
            .cloned()
            .unwrap_or_default();
        let instance = NodeInstance::new(&template);
        lock(&self.instances).insert(id.clone(), instance.clone());
        // The table is dropped with the store, which is the only signal a plugin gets that an
        // instance is gone. A failed push only means the entry outlives the store.
        let _ = shared.table.push(InstanceGuard { id, instances: Arc::downgrade(&self.instances) });
        instance
    }
}

/// Removes an instance's state when its store's resource table is dropped.
struct InstanceGuard {
    id: Arc<str>,
    instances: Weak<Instances>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(instances) = self.instances.upgrade() {
            lock(&instances).remove(&self.id);
        }
    }
}

/// What a `jco:node` host function runs against: the store's resource table and the calling
/// instance's own state.
pub struct NodeView<'a> {
    pub table: &'a mut ResourceTable,
    pub node: Arc<NodeInstance>,
    /// WASI state of the calling component, for the interfaces that share its streams.
    pub ctx: &'a mut wash_runtime::engine::ctx::Ctx,
}

/// Marker selecting [`NodeView`] as the data host functions receive.
pub struct NodeData;

impl HasData for NodeData {
    type Data<'a> = NodeView<'a>;
}

fn view(shared: &mut SharedCtx) -> NodeView<'_> {
    let plugin = shared.active_ctx.get_plugin::<JcoNode>(PLUGIN_ID);
    let node = plugin.attach(shared);
    NodeView { table: &mut shared.table, node, ctx: &mut shared.active_ctx }
}

/// Add every `jco:node` interface to `linker`.
pub fn add_to_linker(linker: &mut Linker<SharedCtx>) -> anyhow::Result<()> {
    node::console::add_to_linker::<_, NodeData>(linker, view)?;
    node::os::add_to_linker::<_, NodeData>(linker, view)?;
    node::process::add_to_linker::<_, NodeData>(linker, view)?;
    node::timers::add_to_linker::<_, NodeData>(linker, view)?;
    Ok(())
}

/// The config key naming interfaces to link without granting: a comma-separated list of
/// interface names, or `*`.
pub const DENY_KEY: &str = "deny";

/// The grants a workload's `host_interfaces` entries for `jco:node` add up to.
///
/// Listing an interface grants it; an entry naming no interface covers the whole package. The
/// host only links what a workload lists, so a component importing an unlisted interface does
/// not start. To start it anyway with that interface refusing every call, as Jco's default
/// providers do, list the interface and name it in the entry's `deny` config.
pub fn grants_from(interfaces: &WitInterfaces<'_>) -> Grants {
    let mut grants = Grants::none();
    let mut denied = Vec::new();
    for interface in interfaces.matching("jco", "node", &[]) {
        if interface.interfaces.is_empty() {
            grants = Grants::all();
        }
        for name in &interface.interfaces {
            if let Some(capability) = Capability::from_interface(name) {
                grants.grant(capability);
            }
        }
        if let Some(list) = interface.config.get(DENY_KEY) {
            denied.extend(list.split(',').map(|name| name.trim().to_owned()));
        }
    }
    if denied.iter().any(|name| name == "*") {
        return Grants::none();
    }
    grants.iter().filter(|capability| !denied.iter().any(|name| name == capability.interface())).collect()
}

#[async_trait::async_trait]
impl HostPlugin for JcoNode {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        let interfaces = Capability::ALL.map(Capability::interface).join(",");
        WitWorld {
            imports: [WitInterface::from(format!("jco:node/{interfaces},types@{PACKAGE_VERSION}").as_str())]
                .into(),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let template = InstanceTemplate {
            grants: grants_from(&interfaces),
            identity: self.identity.clone(),
            environment: item.local_resources().environment.clone().into_iter().collect(),
        };
        lock(&self.templates).insert(item.id().to_owned(), Arc::new(template));
        lock(&self.workloads).entry(item.workload_id().to_owned()).or_default().push(item.id().to_owned());
        add_to_linker(item.linker()?)
    }

    async fn on_workload_unbind(&self, workload_id: &str, _interfaces: WitInterfaces<'_>) -> anyhow::Result<()> {
        // Also reached after a bind that failed part way, so nothing here may assume it finished.
        let components = lock(&self.workloads).remove(workload_id).unwrap_or_default();
        let mut templates = lock(&self.templates);
        for component in components {
            templates.remove(&component);
        }
        Ok(())
    }
}
