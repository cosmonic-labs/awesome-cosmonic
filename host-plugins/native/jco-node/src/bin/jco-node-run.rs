//! Run a component against the `jco:node` host plugin and call its exports.
//!
//! This is the smallest embedder of the plugin: a one-component workload on `wash-runtime`, with
//! the plugin bound to it the way a wasmCloud host binds it. It exists so components built with
//! Jco's Node.js builtin support can be exercised without a JavaScript host.
//!
//! One call:
//!
//! ```console
//! jco-node-run component.wasm --grant os --invoke run --args '[false]'
//! ```
//!
//! Or a session, for components that keep state between calls (a server started by one export
//! and stopped by another). Each line on stdin is a request and gets one line of JSON back on
//! the reply descriptor; between requests the guest's callbacks are delivered:
//!
//! ```console
//! jco-node-run component.wasm --grant http --reply-fd 3
//! {"call": "start", "args": []}      ->  {"ok": 8080}
//! {"call": "stop", "args": []}       ->  {"ok": null}
//! ```

use std::{
    collections::HashMap,
    io::Write as _,
    os::fd::FromRawFd as _,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use anyhow::{Context as _, anyhow, bail};
use jco_node_host::{
    Capability, JcoNode, PLUGIN_ID, ProcessIdentity,
    callbacks::Pump,
    json::{from_json, to_json},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use wash_runtime::{
    engine::{Engine, ctx::SharedCtx},
    host::{HostRef, allowed_hosts::AllowedHost, allowed_ip_name::AllowedIpName},
    plugin::HostPlugin,
    types::{Component, HostPathVolume, LocalResources, Volume, VolumeMount, VolumeType, Workload},
    wasmtime::{
        Store,
        component::{Func, Instance, Val},
    },
    wit::WitInterface,
};
use wasmtime_wasi::WasiCtxBuilder;

const USAGE: &str = "\
Usage: jco-node-run <component.wasm> [options]

Options:
  --grant <list>        jco:node interfaces to grant, comma separated, or `all`
                        (default: none; ungranted interfaces refuse every call)
  --invoke <export>     call one export, print its result as JSON, and exit
  --args <json>         arguments for --invoke, as a JSON array (default: [])
  --reply-fd <fd>       session mode: read requests from stdin, reply on this descriptor
  --env <KEY=VALUE>     set a variable in the guest's environment (repeatable)
  --inherit-env         start the guest's environment from this process's
  --argv <json>         the guest's process.argv, as a JSON array
  --cwd <dir>           the guest's starting directory (default: this process's)
  --node-version <v>    the Node.js version the guest is told it runs on
  --dir <host[::guest]> mount a host directory for the guest's WASI filesystem (repeatable)
  --allow-net           allow outbound connections and name lookups through WASI
";

#[derive(Default)]
struct Options {
    component: Option<PathBuf>,
    grants: Vec<Capability>,
    invoke: Option<String>,
    args: Vec<Value>,
    reply_fd: Option<i32>,
    environment: HashMap<String, String>,
    identity: ProcessIdentity,
    dirs: Vec<(String, String)>,
    allow_net: bool,
}

fn parse_options() -> anyhow::Result<Options> {
    let mut options = Options::default();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = |name: &str| arguments.next().with_context(|| format!("{name} needs a value"));
        match argument.as_str() {
            "--grant" => {
                for name in value("--grant")?.split(',').map(str::trim).filter(|name| !name.is_empty()) {
                    if name == "all" {
                        options.grants = Capability::ALL.to_vec();
                    } else {
                        let capability = Capability::from_interface(name)
                            .with_context(|| format!("jco:node has no interface named {name}"))?;
                        options.grants.push(capability);
                    }
                }
            }
            "--invoke" => options.invoke = Some(value("--invoke")?),
            "--args" => {
                options.args = serde_json::from_str(&value("--args")?).context("--args is not a JSON array")?;
            }
            "--reply-fd" => options.reply_fd = Some(value("--reply-fd")?.parse().context("--reply-fd")?),
            "--env" => {
                let pair = value("--env")?;
                let (key, val) = pair.split_once('=').with_context(|| format!("--env {pair} is not KEY=VALUE"))?;
                options.environment.insert(key.to_owned(), val.to_owned());
            }
            "--inherit-env" => {
                for (key, val) in std::env::vars() {
                    options.environment.entry(key).or_insert(val);
                }
            }
            "--argv" => {
                options.identity.argv =
                    serde_json::from_str(&value("--argv")?).context("--argv is not a JSON array of strings")?;
            }
            "--cwd" => options.identity.cwd = PathBuf::from(value("--cwd")?),
            "--node-version" => options.identity.node_version = value("--node-version")?,
            "--dir" => {
                let spec = value("--dir")?;
                let (host, guest) = spec.split_once("::").unwrap_or((&spec, &spec));
                options.dirs.push((host.to_owned(), guest.to_owned()));
            }
            "--allow-net" => options.allow_net = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with('-') => bail!("unknown option {other}\n\n{USAGE}"),
            other => {
                if options.component.replace(PathBuf::from(other)).is_some() {
                    bail!("only one component can be run\n\n{USAGE}");
                }
            }
        }
    }
    if options.component.is_none() {
        bail!("no component given\n\n{USAGE}");
    }
    Ok(options)
}

/// A live instance and the means to call it.
struct Session {
    store: Store<SharedCtx>,
    instance: Instance,
    pump: Pump,
}

/// How a call into the guest ended, other than by returning.
enum Stop {
    /// The guest exited, through `process.exit` or WASI.
    Exit(i32),
    /// The guest trapped; the instance cannot be entered again.
    Trap(anyhow::Error),
}

impl Session {
    async fn start(options: &Options) -> anyhow::Result<Self> {
        let path = options.component.as_deref().context("no component given")?;
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

        // A host links only the interfaces a workload lists, so every interface is listed and
        // the ones not granted are named in `deny`: they link, and refuse every call.
        let listed = Capability::ALL.map(Capability::interface).join(",");
        let denied: Vec<&str> = Capability::ALL
            .into_iter()
            .filter(|capability| !options.grants.contains(capability))
            .map(Capability::interface)
            .collect();
        let mut interface = WitInterface::from(format!("jco:node/{listed},types@0.1.0").as_str());
        interface.config.insert(jco_node_host::plugin::DENY_KEY.to_owned(), denied.join(","));

        let mut volumes = Vec::new();
        let mut volume_mounts = Vec::new();
        for (index, (host, guest)) in options.dirs.iter().enumerate() {
            let name = format!("dir-{index}");
            volumes.push(Volume {
                name: name.clone(),
                volume_type: VolumeType::HostPath(HostPathVolume { local_path: host.clone() }),
            });
            volume_mounts.push(VolumeMount { name, mount_path: guest.clone(), read_only: false });
        }

        let mut local_resources =
            LocalResources { environment: options.environment.clone(), volume_mounts, ..Default::default() };
        if options.allow_net {
            local_resources.allowed_hosts = Arc::from([AllowedHost::Any]);
            local_resources.allowed_ip_name_lookups = Arc::from([AllowedIpName::Any]);
        }

        wash_runtime::init_crypto();
        let engine = Engine::builder()
            .with_pooling_allocator(false)
            .with_allowed_host_paths(options.dirs.iter().map(|(host, _)| PathBuf::from(host)))
            .build()?;
        let plugin: Arc<dyn HostPlugin> = Arc::new(JcoNode::new().with_identity(options.identity.clone()));
        let plugins = HashMap::from([(PLUGIN_ID, plugin)]);
        let resolved = engine
            .initialize_workload(
                "jco-node-run",
                Workload {
                    name: "jco-node-run".into(),
                    namespace: "default".into(),
                    annotations: HashMap::new(),
                    service: None,
                    volumes,
                    host_interfaces: vec![interface],
                    components: vec![Component {
                        name: "component".into(),
                        bytes: bytes.into(),
                        local_resources,
                        ..Default::default()
                    }],
                },
            )?
            .resolve(Some(&plugins), &Default::default(), &HostRef::detached(), &Default::default())
            .await?;
        let id = resolved
            .components()
            .read_owned()
            .await
            .keys()
            .next()
            .cloned()
            .context("the workload resolved without its component")?;

        let pre = resolved.instantiate_pre(&id).await?;
        let mut store = resolved.new_store(&id).await?;
        // Give the guest this process's standard streams, keeping the filesystem and network
        // state the workload resolved with.
        let mut cli = WasiCtxBuilder::new();
        let environment: Vec<(&str, &str)> =
            options.environment.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
        cli.envs(&environment).args(&options.identity.argv).inherit_stdio();
        *store.data_mut().active_ctx.ctx.cli() = std::mem::take(cli.build().cli());

        let instance = pre.instantiate_async(&mut store).await?;
        let pump = Pump::attach(&mut store, &instance)?;
        Ok(Self { store, instance, pump })
    }

    /// Find an export by name: `run` for a function of the world, `ns:pkg/iface@1.0.0#fn` for a
    /// function of an exported interface.
    fn export(&mut self, name: &str) -> anyhow::Result<Func> {
        let index = match name.split_once('#') {
            Some((interface, function)) => {
                let parent = self
                    .instance
                    .get_export_index(&mut self.store, None, interface)
                    .with_context(|| format!("the component does not export {interface}"))?;
                self.instance.get_export_index(&mut self.store, Some(&parent), function)
            }
            None => self.instance.get_export_index(&mut self.store, None, name),
        }
        .with_context(|| format!("the component does not export {name}"))?;
        self.instance.get_func(&mut self.store, index).with_context(|| format!("{name} is not a function"))
    }

    async fn call(&mut self, name: &str, args: &[Value]) -> Result<Value, Stop> {
        let prepared = (|| -> anyhow::Result<(Func, Vec<Val>, usize)> {
            let func = self.export(name)?;
            let ty = func.ty(&self.store);
            let types: Vec<_> = ty.params().collect();
            if types.len() != args.len() {
                bail!("{name} takes {} arguments, {} given", types.len(), args.len());
            }
            let params = args
                .iter()
                .zip(&types)
                .map(|(value, (param, ty))| from_json(value, ty).with_context(|| format!("argument {param}")))
                .collect::<anyhow::Result<_>>()?;
            Ok((func, params, ty.results().len()))
        })();
        let (func, params, result_count) = prepared.map_err(Stop::Trap)?;
        let mut results = vec![Val::Bool(false); result_count];
        if let Err(error) = func.call_async(&mut self.store, &params, &mut results).await {
            let error = anyhow::Error::from(error);
            return Err(match error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                Some(exit) => Stop::Exit(exit.0),
                None => Stop::Trap(error),
            });
        }
        match results.first() {
            Some(value) => to_json(value).map_err(Stop::Trap),
            None => Ok(Value::Null),
        }
    }

    /// Deliver callbacks that are already due, so a call sees their effects.
    async fn drain(&mut self) -> Result<(), Stop> {
        while let Some(job) = self.pump.try_next() {
            self.deliver(job).await?;
        }
        Ok(())
    }

    async fn deliver(&mut self, job: jco_node_host::callbacks::Job) -> Result<(), Stop> {
        self.pump.deliver(&mut self.store, job).await.map_err(|error| {
            match error.downcast_ref::<wasmtime_wasi::I32Exit>() {
                Some(exit) => Stop::Exit(exit.0),
                None => Stop::Trap(error),
            }
        })
    }
}

fn status(code: i32) -> ExitCode {
    ExitCode::from(code.clamp(0, 255) as u8)
}

async fn run() -> anyhow::Result<ExitCode> {
    let options = parse_options()?;
    let mut session = Session::start(&options).await?;

    if let Some(export) = &options.invoke {
        return Ok(match session.call(export, &options.args).await {
            Ok(value) => {
                println!("{value}");
                ExitCode::SUCCESS
            }
            Err(Stop::Exit(code)) => status(code),
            Err(Stop::Trap(error)) => {
                eprintln!("{error:?}");
                ExitCode::from(70)
            }
        });
    }

    let fd = options.reply_fd.context("give --invoke for one call or --reply-fd for a session")?;
    // SAFETY: the caller passed this descriptor for the replies and nothing else here uses it.
    let mut replies = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut reply = move |value: Value| -> anyhow::Result<()> {
        writeln!(replies, "{value}").context("writing a reply")?;
        replies.flush().context("writing a reply")
    };
    let mut requests = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            biased;
            line = requests.next_line() => {
                let Some(line) = line.context("reading a request")? else {
                    return Ok(ExitCode::SUCCESS);
                };
                if line.trim().is_empty() {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).context("a request is not JSON")?;
                let name = request.get("call").and_then(Value::as_str).ok_or_else(|| anyhow!("a request needs a call"))?;
                let args = request.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
                let outcome = match session.drain().await {
                    Ok(()) => session.call(name, &args).await,
                    Err(stop) => Err(stop),
                };
                match outcome {
                    Ok(value) => reply(json!({ "ok": value }))?,
                    Err(Stop::Exit(code)) => {
                        reply(json!({ "exit": code }))?;
                        return Ok(status(code));
                    }
                    Err(Stop::Trap(error)) => {
                        reply(json!({ "trap": format!("{error:?}") }))?;
                        return Ok(ExitCode::from(70));
                    }
                }
            }
            job = session.pump.next() => {
                let Some(job) = job else { continue };
                match session.deliver(job).await {
                    Ok(()) => {}
                    Err(Stop::Exit(code)) => return Ok(status(code)),
                    Err(Stop::Trap(error)) => {
                        eprintln!("{error:?}");
                        return Ok(ExitCode::from(70));
                    }
                }
            }
        }
    }
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("jco-node-run: {error}");
            return ExitCode::from(2);
        }
    };
    let code = match runtime.block_on(run()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("jco-node-run: {error:?}");
            ExitCode::from(2)
        }
    };
    // A guest that exits mid-session leaves the stdin reader parked in a blocking read, and
    // waiting for that thread would hold the process open until the caller closed stdin.
    runtime.shutdown_background();
    code
}
