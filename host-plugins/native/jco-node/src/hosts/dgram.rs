//! `jco:node/dgram`: UDP sockets.
//!
//! Each socket reports what happens to it (datagrams, errors, finished sends and lookups) to the
//! guest listener it was created with, through the instance's job queue, the way Jco's Node
//! provider emits them through `dgram-callbacks`. Calls that Node makes synchronously
//! (`bindSync`, `connectSync`, the option setters) complete before they return.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use socket2::{Domain, Protocol, SockAddr, Socket as RawSocket, Type};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};
use wash_runtime::wasmtime::{self, component::Resource};

use crate::{
    bindings::jco::node::dgram::{
        AddressInfo, BufferErrorInfo, Errno, Error, Host, HostSocket, MembershipAction, Options, SocketOption,
        SocketQuery, SocketType,
    },
    callbacks::{DgramEvent, Job},
    errno,
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

type DgramResult<T> = wasmtime::Result<Result<T, Error>>;

fn coded(name: &str, code: &str, message: impl Into<String>) -> Error {
    Error {
        name: name.into(),
        message: message.into(),
        code: Some(code.into()),
        errno: None,
        syscall: None,
        address: None,
        port: None,
        info: None,
    }
}

/// A libuv failure as Node's `exceptionWithHostPort` words it: `bind EADDRINUSE 127.0.0.1:41234`.
pub(crate) fn uv_error(error: &io::Error, syscall: &str, at: Option<(&str, u16)>) -> Error {
    let uv = errno::from_io(error);
    let message = match at {
        Some((address, port)) if port > 0 => format!("{syscall} {} {address}:{port}", uv.code),
        Some((address, _)) => format!("{syscall} {} {address}", uv.code),
        None => format!("{syscall} {}", uv.code),
    };
    Error {
        name: "Error".into(),
        message,
        code: Some(uv.code.into()),
        errno: Some(Errno::Number(uv.node_errno())),
        syscall: Some(syscall.into()),
        address: at.map(|(address, _)| address.into()),
        port: at.map(|(_, port)| port).filter(|port| *port > 0),
        info: None,
    }
}

/// `ERR_SOCKET_BUFFER_SIZE`, a `SystemError` with the libuv failure in `info`.
fn buffer_size_error(error: &io::Error, receive: bool) -> Error {
    let uv = errno::from_io(error);
    let syscall = if receive { "uv_recv_buffer_size" } else { "uv_send_buffer_size" };
    Error {
        name: "SystemError".into(),
        message: format!("Could not get or set buffer size: {syscall} returned {} ({})", uv.code, uv.description),
        code: Some("ERR_SOCKET_BUFFER_SIZE".into()),
        errno: None,
        syscall: None,
        address: None,
        port: None,
        info: Some(BufferErrorInfo {
            errno: uv.node_errno() as f64,
            code: uv.code.into(),
            message: uv.description.into(),
            syscall: syscall.into(),
        }),
    }
}

fn not_running() -> Error {
    coded("Error", "ERR_SOCKET_DGRAM_NOT_RUNNING", "Not running")
}

fn address_info(address: SocketAddr) -> AddressInfo {
    AddressInfo {
        address: address.ip().to_string(),
        family: if address.is_ipv4() { "IPv4" } else { "IPv6" }.into(),
        port: address.port(),
    }
}

pub struct DgramSocket {
    kind: SocketType,
    options: Options,
    listener: u32,
    jobs: mpsc::UnboundedSender<Job>,
    /// Created at bind; until then the socket only exists on the guest's side.
    socket: Option<Arc<UdpSocket>>,
    receiver: Option<JoinHandle<()>>,
    remote: Option<SocketAddr>,
    closed: bool,
}

impl DgramSocket {
    fn unspecified(&self) -> IpAddr {
        match self.kind {
            SocketType::Udp4 => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            SocketType::Udp6 => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        }
    }

    fn parse(&self, address: &str) -> Option<IpAddr> {
        let ip = address.parse::<IpAddr>().ok()?;
        match (self.kind, ip) {
            (SocketType::Udp4, IpAddr::V4(_)) | (SocketType::Udp6, IpAddr::V6(_)) => Some(ip),
            // A udp6 socket reaches IPv4 peers through mapped addresses.
            (SocketType::Udp6, IpAddr::V4(v4)) => Some(IpAddr::V6(v4.to_ipv6_mapped())),
            _ => None,
        }
    }

    fn bind(&mut self, address: &str, port: u16) -> Result<AddressInfo, Error> {
        if self.closed {
            return Err(not_running());
        }
        if self.socket.is_some() {
            return Err(coded("Error", "ERR_SOCKET_ALREADY_BOUND", "Socket is already bound"));
        }
        let ip = if address.is_empty() { Some(self.unspecified()) } else { self.parse(address) };
        let Some(ip) = ip else {
            return Err(uv_error(&io::Error::from_raw_os_error(libc::EINVAL), "bind", Some((address, port))));
        };
        let domain = if ip.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
        let open = || -> io::Result<UdpSocket> {
            let socket = RawSocket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
            if self.options.reuse_addr {
                socket.set_reuse_address(true)?;
            }
            if self.options.reuse_port {
                socket.set_reuse_port(true)?;
            }
            if ip.is_ipv6() {
                socket.set_only_v6(self.options.ipv6_only)?;
            }
            socket.set_nonblocking(true)?;
            socket.bind(&SockAddr::from(SocketAddr::new(ip, port)))?;
            UdpSocket::from_std(socket.into())
        };
        let socket = open().map_err(|error| uv_error(&error, "bind", Some((&ip.to_string(), port))))?;
        let local = socket.local_addr().map_err(|error| uv_error(&error, "bind", None))?;
        let socket = Arc::new(socket);
        self.socket = Some(socket.clone());
        let jobs = self.jobs.clone();
        let listener = self.listener;
        self.receiver = Some(tokio::spawn(async move {
            let mut buffer = vec![0u8; 65_536];
            loop {
                let event = match socket.recv_from(&mut buffer).await {
                    Ok((read, from)) => DgramEvent::Message { data: buffer[..read].to_vec(), remote: from },
                    // An ICMP error from an earlier send surfaces here on Linux; Node reports
                    // those through `recvmsg` errors, which it emits as `error` events.
                    Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => continue,
                    Err(error) => DgramEvent::Error(uv_error(&error, "recvmsg", None)),
                };
                if jobs.send(Job::Dgram { listener, event }).is_err() {
                    return;
                }
            }
        }));
        Ok(address_info(local))
    }

    fn bound(&mut self) -> Result<Arc<UdpSocket>, Error> {
        if self.closed {
            return Err(not_running());
        }
        if self.socket.is_none() {
            // Node binds an unbound socket to the wildcard address on first use.
            let unspecified = self.unspecified().to_string();
            self.bind(&unspecified, 0)?;
        }
        Ok(self.socket.clone().expect("bound above"))
    }

    fn raw(&self) -> Result<socket2::SockRef<'_>, Error> {
        match &self.socket {
            Some(socket) if !self.closed => Ok(socket2::SockRef::from(socket.as_ref())),
            // libuv reports EBADF for options on a socket with no descriptor yet.
            _ => Err(io::Error::from_raw_os_error(libc::EBADF)).map_err(|error| uv_error(&error, "setsockopt", None)),
        }
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if let Some(receiver) = self.receiver.take() {
            receiver.abort();
        }
        self.socket = None;
        // Queued after every event already reported, so finished sends still reach the guest.
        let _ = self.jobs.send(Job::DgramRetire { listener: self.listener });
    }
}

impl Drop for DgramSocket {
    fn drop(&mut self) {
        self.close();
    }
}

impl Host for NodeView<'_> {
    async fn create_socket(&mut self, options: Options, listener: u32) -> DgramResult<Resource<DgramSocket>> {
        if !self.node.allows(Capability::Dgram) {
            return Ok(Err(coded("Error", &Capability::Dgram.denied_code(), denied_message(Capability::Dgram))));
        }
        let socket = DgramSocket {
            kind: options.type_,
            options,
            listener,
            jobs: self.node.jobs.clone(),
            socket: None,
            receiver: None,
            remote: None,
            closed: false,
        };
        Ok(Ok(self.table.push(socket)?))
    }
}

impl HostSocket for NodeView<'_> {
    async fn bind(&mut self, self_: Resource<DgramSocket>, address: String, port: u16) -> DgramResult<AddressInfo> {
        Ok(self.table.get_mut(&self_)?.bind(&address, port))
    }

    async fn connect(&mut self, self_: Resource<DgramSocket>, address: String, port: u16) -> DgramResult<()> {
        let socket = self.table.get_mut(&self_)?;
        if socket.remote.is_some() {
            return Ok(Err(coded("Error", "ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected")));
        }
        let Some(ip) = socket.parse(&address) else {
            return Ok(Err(uv_error(&io::Error::from_raw_os_error(libc::EINVAL), "connect", Some((&address, port)))));
        };
        let udp = match socket.bound() {
            Ok(udp) => udp,
            Err(error) => return Ok(Err(error)),
        };
        let remote = SocketAddr::new(ip, port);
        if let Err(error) = socket2::SockRef::from(udp.as_ref()).connect(&SockAddr::from(remote)) {
            return Ok(Err(uv_error(&error, "connect", Some((&address, port)))));
        }
        socket.remote = Some(remote);
        Ok(Ok(()))
    }

    async fn disconnect(&mut self, self_: Resource<DgramSocket>) -> DgramResult<()> {
        let socket = self.table.get_mut(&self_)?;
        if socket.remote.is_none() {
            return Ok(Err(coded("Error", "ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected")));
        }
        let raw = match socket.raw() {
            Ok(raw) => raw,
            Err(error) => return Ok(Err(error)),
        };
        // Connecting to AF_UNSPEC dissolves the association.
        // SAFETY: plain data, valid when zeroed.
        let mut unspec: libc::sockaddr = unsafe { std::mem::zeroed() };
        unspec.sa_family = libc::AF_UNSPEC as libc::sa_family_t;
        // SAFETY: a valid descriptor and address of the given length.
        let status = unsafe {
            libc::connect(
                std::os::fd::AsRawFd::as_raw_fd(&*raw),
                &unspec,
                size_of::<libc::sockaddr>() as libc::socklen_t,
            )
        };
        if status != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EAFNOSUPPORT) {
                return Ok(Err(uv_error(&error, "disconnect", None)));
            }
        }
        socket.remote = None;
        Ok(Ok(()))
    }

    async fn resolve(&mut self, self_: Resource<DgramSocket>, address: String, id: u32) -> wasmtime::Result<()> {
        let socket = self.table.get(&self_)?;
        let ipv6 = matches!(socket.kind, SocketType::Udp6);
        let jobs = socket.jobs.clone();
        let listener = socket.listener;
        tokio::spawn(async move {
            let result = match address.parse::<IpAddr>() {
                Ok(ip) => Ok(ip.to_string()),
                Err(_) => match tokio::net::lookup_host((address.as_str(), 0)).await {
                    Ok(found) => found
                        .map(|found| found.ip())
                        .find(|ip| ip.is_ipv6() == ipv6)
                        .map(|ip| ip.to_string())
                        .ok_or(()),
                    Err(_) => Err(()),
                },
            };
            let result = result.map_err(|()| Error {
                errno: Some(Errno::Number(-3008)),
                syscall: Some("getaddrinfo".into()),
                ..coded("Error", "ENOTFOUND", format!("getaddrinfo ENOTFOUND {address}"))
            });
            let _ = jobs.send(Job::Dgram { listener, event: DgramEvent::Resolved { id, result } });
        });
        Ok(())
    }

    async fn send(
        &mut self,
        self_: Resource<DgramSocket>,
        data: Vec<u8>,
        port: Option<u16>,
        address: Option<String>,
        callback: Option<u32>,
    ) -> DgramResult<()> {
        let socket = self.table.get_mut(&self_)?;
        let target = match port {
            Some(port) => {
                if socket.remote.is_some() {
                    return Ok(Err(coded("Error", "ERR_SOCKET_DGRAM_IS_CONNECTED", "Already connected")));
                }
                let address = address.unwrap_or_else(|| match socket.kind {
                    SocketType::Udp4 => "127.0.0.1".into(),
                    SocketType::Udp6 => "::1".into(),
                });
                let Some(ip) = socket.parse(&address) else {
                    return Ok(Err(uv_error(&io::Error::from_raw_os_error(libc::EINVAL), "send", Some((&address, port)))));
                };
                Some(SocketAddr::new(ip, port))
            }
            None if socket.remote.is_none() => {
                return Ok(Err(coded("Error", "ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected")));
            }
            None => None,
        };
        let udp = match socket.bound() {
            Ok(udp) => udp,
            Err(error) => return Ok(Err(error)),
        };
        let jobs = socket.jobs.clone();
        let listener = socket.listener;
        let report = move |result: io::Result<usize>| {
            let Some(id) = callback else { return };
            let result = result.map(|sent| sent as u32).map_err(|error| {
                let at = target.map(|target| (target.ip().to_string(), target.port()));
                uv_error(&error, "send", at.as_ref().map(|(address, port)| (address.as_str(), *port)))
            });
            let _ = jobs.send(Job::Dgram { listener, event: DgramEvent::Sent { id, result } });
        };
        let attempt = match target {
            Some(target) => udp.try_send_to(&data, target),
            None => udp.try_send(&data),
        };
        match attempt {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                tokio::spawn(async move {
                    let result = match target {
                        Some(target) => udp.send_to(&data, target).await,
                        None => udp.send(&data).await,
                    };
                    report(result);
                });
            }
            result => report(result),
        }
        Ok(Ok(()))
    }

    async fn address(&mut self, self_: Resource<DgramSocket>, remote: bool) -> DgramResult<AddressInfo> {
        let socket = self.table.get(&self_)?;
        if remote {
            return Ok(socket
                .remote
                .map(|remote| address_info(remote))
                .ok_or_else(|| coded("Error", "ERR_SOCKET_DGRAM_NOT_CONNECTED", "Not connected")));
        }
        Ok(match &socket.socket {
            Some(udp) if !socket.closed => udp.local_addr().map(address_info).map_err(|error| uv_error(&error, "getsockname", None)),
            _ => Err(uv_error(&io::Error::from_raw_os_error(libc::EBADF), "getsockname", None)),
        })
    }

    async fn set_option(&mut self, self_: Resource<DgramSocket>, option: SocketOption) -> DgramResult<()> {
        let socket = self.table.get(&self_)?;
        let ipv6 = matches!(socket.kind, SocketType::Udp6);
        let receive = matches!(option, SocketOption::RecvBuffer(_));
        let syscall = match &option {
            SocketOption::Broadcast(_) => "setBroadcast",
            SocketOption::MulticastLoopback(_) => "setMulticastLoopback",
            SocketOption::Ttl(_) => "setTTL",
            SocketOption::MulticastTtl(_) => "setMulticastTTL",
            SocketOption::RecvBuffer(_) | SocketOption::SendBuffer(_) => "",
            SocketOption::MulticastInterface(_) => "setMulticastInterface",
        };
        let raw = match socket.raw() {
            Ok(raw) => raw,
            Err(_) if syscall.is_empty() => {
                return Ok(Err(buffer_size_error(&io::Error::from_raw_os_error(libc::EBADF), receive)));
            }
            Err(_) => return Ok(Err(uv_error(&io::Error::from_raw_os_error(libc::EBADF), syscall, None))),
        };
        let ttl = |value: f64| -> io::Result<u32> {
            if value.fract() != 0.0 || !(1.0..=255.0).contains(&value) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            Ok(value as u32)
        };
        let result = match option {
            SocketOption::Broadcast(on) => raw.set_broadcast(on),
            SocketOption::MulticastLoopback(on) if ipv6 => raw.set_multicast_loop_v6(on),
            SocketOption::MulticastLoopback(on) => raw.set_multicast_loop_v4(on),
            SocketOption::Ttl(value) if ipv6 => ttl(value).and_then(|ttl| raw.set_unicast_hops_v6(ttl)),
            SocketOption::Ttl(value) => ttl(value).and_then(|ttl| raw.set_ttl_v4(ttl)),
            SocketOption::MulticastTtl(value) if ipv6 => ttl(value).and_then(|ttl| raw.set_multicast_hops_v6(ttl)),
            SocketOption::MulticastTtl(value) => ttl(value).and_then(|ttl| raw.set_multicast_ttl_v4(ttl)),
            SocketOption::RecvBuffer(size) => {
                return Ok(raw.set_recv_buffer_size(size as usize).map_err(|error| buffer_size_error(&error, true)));
            }
            SocketOption::SendBuffer(size) => {
                return Ok(raw.set_send_buffer_size(size as usize).map_err(|error| buffer_size_error(&error, false)));
            }
            SocketOption::MulticastInterface(interface) if ipv6 => {
                // An IPv6 interface is named by its scope: `::%eth0` or `::%2`.
                let scope = interface.split_once('%').map_or("", |(_, scope)| scope).to_owned();
                let index = scope.parse::<u32>().ok().or_else(|| {
                    let name = std::ffi::CString::new(scope).ok()?;
                    // SAFETY: a valid C string.
                    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
                    (index != 0).then_some(index)
                });
                match index {
                    Some(index) => raw.set_multicast_if_v6(index),
                    None => Err(io::Error::from_raw_os_error(libc::EINVAL)),
                }
            }
            SocketOption::MulticastInterface(interface) => match interface.parse::<Ipv4Addr>() {
                Ok(ip) => raw.set_multicast_if_v4(&ip),
                Err(_) => Err(io::Error::from_raw_os_error(libc::EINVAL)),
            },
        };
        Ok(result.map_err(|error| uv_error(&error, syscall, None)))
    }

    async fn query(&mut self, self_: Resource<DgramSocket>, query: SocketQuery) -> DgramResult<f64> {
        let socket = self.table.get(&self_)?;
        let receive = matches!(query, SocketQuery::RecvBuffer);
        Ok(match query {
            // Sends complete or are handed to a task at once, so nothing waits in a queue.
            SocketQuery::SendQueueSize | SocketQuery::SendQueueCount => Ok(0.0),
            SocketQuery::RecvBuffer | SocketQuery::SendBuffer => match socket.raw() {
                Ok(raw) => {
                    let size = if receive { raw.recv_buffer_size() } else { raw.send_buffer_size() };
                    size.map(|size| size as f64).map_err(|error| buffer_size_error(&error, receive))
                }
                Err(_) => Err(buffer_size_error(&io::Error::from_raw_os_error(libc::EBADF), receive)),
            },
        })
    }

    async fn membership(
        &mut self,
        self_: Resource<DgramSocket>,
        action: MembershipAction,
        group: String,
        source: Option<String>,
        iface: Option<String>,
    ) -> DgramResult<()> {
        let socket = self.table.get_mut(&self_)?;
        let syscall = match action {
            MembershipAction::Add => "addMembership",
            MembershipAction::Drop => "dropMembership",
            MembershipAction::AddSource => "addSourceSpecificMembership",
            MembershipAction::DropSource => "dropSourceSpecificMembership",
        };
        // Node binds an unbound socket before joining a group.
        if let Err(error) = socket.bound() {
            return Ok(Err(error));
        }
        let raw = match socket.raw() {
            Ok(raw) => raw,
            Err(error) => return Ok(Err(error)),
        };
        let einval = || io::Error::from_raw_os_error(libc::EINVAL);
        let interface_v4 = || iface.as_deref().map_or(Ok(Ipv4Addr::UNSPECIFIED), |iface| iface.parse().map_err(|_| einval()));
        let result = match group.parse::<IpAddr>() {
            Ok(IpAddr::V4(group)) => match action {
                MembershipAction::Add => interface_v4().and_then(|iface| raw.join_multicast_v4(&group, &iface)),
                MembershipAction::Drop => interface_v4().and_then(|iface| raw.leave_multicast_v4(&group, &iface)),
                MembershipAction::AddSource | MembershipAction::DropSource => {
                    match source.as_deref().map(str::parse::<Ipv4Addr>) {
                        Some(Ok(source)) => interface_v4().and_then(|iface| {
                            if matches!(action, MembershipAction::AddSource) {
                                raw.join_ssm_v4(&source, &group, &iface)
                            } else {
                                raw.leave_ssm_v4(&source, &group, &iface)
                            }
                        }),
                        _ => Err(einval()),
                    }
                }
            },
            Ok(IpAddr::V6(group)) => {
                let index = iface
                    .as_deref()
                    .and_then(|iface| iface.split_once('%'))
                    .and_then(|(_, scope)| scope.parse::<u32>().ok())
                    .unwrap_or(0);
                match action {
                    MembershipAction::Add => raw.join_multicast_v6(&group, index),
                    MembershipAction::Drop => raw.leave_multicast_v6(&group, index),
                    _ => Err(io::Error::from_raw_os_error(libc::ENOSYS)),
                }
            }
            Err(_) => Err(einval()),
        };
        Ok(result.map_err(|error| uv_error(&error, syscall, None)))
    }

    async fn close(&mut self, self_: Resource<DgramSocket>) -> wasmtime::Result<()> {
        self.table.get_mut(&self_)?.close();
        Ok(())
    }

    // A host has no event loop for a socket to keep alive.
    async fn set_ref(&mut self, _self_: Resource<DgramSocket>, _ref_: bool) -> wasmtime::Result<()> {
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<DgramSocket>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}
