//! `jco:node/dns`: name resolution.
//!
//! `lookup` and `lookup-service` go through the system resolver (`getaddrinfo`, `getnameinfo`),
//! as Node's do. The `resolve*` family queries DNS servers directly, as Node does through
//! c-ares: the servers in `/etc/resolv.conf` unless a resolver names its own, over UDP with a
//! TCP retry for truncated answers. Errors carry Node's shapes: `getaddrinfo ENOTFOUND host`
//! with a numeric errno for the system resolver, `queryA ENODATA host` without one for c-ares.

use std::{
    ffi::{CStr, CString},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpStream, UdpSocket},
};
use wash_runtime::wasmtime;

use crate::{
    bindings::jco::node::dns::{
        AddressWithTtl, AnyRecord, CaaRecord, Errno, Error, Family, Host, LookupAddress, LookupOptions,
        LookupServiceResult, MxRecord, NaptrRecord, ResolverConfig, ResultOrder, SoaRecord, SrvRecord, TlsaRecord,
    },
    hosts::denied_message,
    plugin::NodeView,
    state::Capability,
};

type DnsResult<T> = wasmtime::Result<Result<T, Error>>;

macro_rules! granted {
    ($view:expr) => {
        if !$view.node.allows(Capability::Dns) {
            return Ok(Err(Error {
                name: "Error".into(),
                message: denied_message(Capability::Dns),
                code: Some(Capability::Dns.denied_code().into()),
                errno: None,
                syscall: None,
                hostname: None,
            }));
        }
    };
}

/// A c-ares failure: `queryA ENODATA example.com`.
fn ares(code: &str, syscall: &str, hostname: &str) -> Error {
    Error {
        name: "Error".into(),
        message: format!("{syscall} {code} {hostname}"),
        code: Some(code.into()),
        errno: None,
        syscall: Some(syscall.into()),
        hostname: Some(hostname.into()),
    }
}

/// A `getaddrinfo`/`getnameinfo` failure, with the libuv errno Node reports for it.
fn system(eai: i32, syscall: &str, subject: &str) -> Error {
    let (code, errno) = match eai {
        libc::EAI_NONAME | libc::EAI_NODATA => ("ENOTFOUND", -3008),
        libc::EAI_AGAIN => ("EAI_AGAIN", -3001),
        libc::EAI_FAIL => ("EAI_FAIL", -3007),
        libc::EAI_FAMILY => ("EAI_FAMILY", -3006),
        libc::EAI_MEMORY => ("EAI_MEMORY", -3005),
        libc::EAI_SERVICE => ("EAI_SERVICE", -3010),
        libc::EAI_SOCKTYPE => ("EAI_SOCKTYPE", -3011),
        libc::EAI_BADFLAGS => ("EAI_BADFLAGS", -3002),
        _ => ("EAI_FAIL", -3007),
    };
    Error {
        name: "Error".into(),
        message: format!("{syscall} {code} {subject}"),
        code: Some(code.into()),
        errno: Some(Errno::Number(errno)),
        syscall: Some(syscall.into()),
        hostname: (syscall == "getaddrinfo").then(|| subject.into()),
    }
}

fn invalid_ip(value: &str) -> Error {
    Error {
        name: "TypeError".into(),
        message: format!("Invalid IP address: {value}"),
        code: Some("ERR_INVALID_IP_ADDRESS".into()),
        errno: None,
        syscall: None,
        hostname: None,
    }
}

// ---- Servers ----------------------------------------------------------------------------------

/// Parse a server the way `Resolver.setServers` does: `ip`, `ip:port`, `[ipv6]` or `[ipv6]:port`.
fn parse_server(value: &str) -> Option<SocketAddr> {
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, 53));
    }
    if let Some(rest) = value.strip_prefix('[') {
        let (ip, rest) = rest.split_once(']')?;
        let ip: Ipv6Addr = ip.parse().ok()?;
        let port = match rest {
            "" => 53,
            rest => rest.strip_prefix(':')?.parse().ok()?,
        };
        return Some(SocketAddr::new(IpAddr::V6(ip), port));
    }
    let (ip, port) = value.rsplit_once(':')?;
    Some(SocketAddr::new(IpAddr::V4(ip.parse().ok()?), port.parse().ok()?))
}

/// A server as `getServers` prints it: the port only when it is not 53.
fn format_server(server: SocketAddr) -> String {
    match (server.ip(), server.port()) {
        (ip, 53) => ip.to_string(),
        (IpAddr::V4(ip), port) => format!("{ip}:{port}"),
        (IpAddr::V6(ip), port) => format!("[{ip}]:{port}"),
    }
}

fn system_servers() -> Vec<SocketAddr> {
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let servers: Vec<_> = conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| {
            // Drop an IPv6 zone, which c-ares keeps out of `getServers`.
            let ip = rest.trim().split('%').next()?;
            ip.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 53))
        })
        .collect();
    if servers.is_empty() { vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53)] } else { servers }
}

struct Resolver {
    servers: Vec<SocketAddr>,
    timeout: Duration,
    tries: u32,
    local: Option<(String, String)>,
}

impl Resolver {
    fn new(config: Option<ResolverConfig>) -> Result<Self, Error> {
        let mut resolver = Self { servers: system_servers(), timeout: Duration::from_millis(2000), tries: 4, local: None };
        let Some(config) = config else { return Ok(resolver) };
        if let Some(timeout) = config.options.timeout.filter(|timeout| *timeout >= 0) {
            resolver.timeout = Duration::from_millis(timeout as u64);
        }
        if let Some(tries) = config.options.tries {
            resolver.tries = tries.max(1);
        }
        if let Some(max) = config.options.max_timeout.filter(|max| *max > 0) {
            resolver.timeout = resolver.timeout.min(Duration::from_millis(u64::from(max)));
        }
        if let Some(servers) = config.servers {
            resolver.servers =
                servers.iter().map(|server| parse_server(server).ok_or_else(|| invalid_ip(server))).collect::<Result<_, _>>()?;
        }
        resolver.local = config.local_address;
        Ok(resolver)
    }

    fn local_for(&self, server: SocketAddr) -> SocketAddr {
        let pick = |ip: &str| ip.parse::<IpAddr>().ok();
        let chosen = self.local.as_ref().and_then(|(v4, v6)| if server.is_ipv4() { pick(v4) } else { pick(v6) });
        match chosen {
            Some(ip) => SocketAddr::new(ip, 0),
            None if server.is_ipv4() => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            None => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        }
    }

    /// Ask the servers in turn until one answers; the answer's code decides the error.
    async fn query(&self, name: &str, kind: u16) -> Result<Message, &'static str> {
        if self.servers.is_empty() {
            return Err("ECONNREFUSED");
        }
        let packet = build_query(name, kind);
        let mut last = "ETIMEOUT";
        for _ in 0..self.tries {
            for server in &self.servers {
                match self.ask(*server, &packet).await {
                    Ok(message) => match message.rcode {
                        // A server that fails or refuses gets no say over the others.
                        2 => last = "ESERVFAIL",
                        5 => last = "EREFUSED",
                        _ => return Ok(message),
                    },
                    Err(error) => {
                        last = match error.kind() {
                            io::ErrorKind::TimedOut => "ETIMEOUT",
                            io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
                            io::ErrorKind::InvalidData => "EBADRESP",
                            _ => "ECONNREFUSED",
                        }
                    }
                }
            }
        }
        Err(last)
    }

    async fn ask(&self, server: SocketAddr, packet: &[u8]) -> io::Result<Message> {
        let id = u16::from_be_bytes([packet[0], packet[1]]);
        let udp = async {
            let socket = UdpSocket::bind(self.local_for(server)).await?;
            socket.connect(server).await?;
            socket.send(packet).await?;
            let mut buffer = vec![0u8; 4096];
            loop {
                let read = socket.recv(&mut buffer).await?;
                let message = parse_message(&buffer[..read])?;
                if message.id == id {
                    return Ok::<_, io::Error>(message);
                }
            }
        };
        let message = tokio::time::timeout(self.timeout, udp).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        if !message.truncated {
            return Ok(message);
        }
        let tcp = async {
            let mut stream = TcpStream::connect(server).await?;
            let mut framed = (packet.len() as u16).to_be_bytes().to_vec();
            framed.extend_from_slice(packet);
            stream.write_all(&framed).await?;
            let length = stream.read_u16().await?;
            let mut buffer = vec![0u8; usize::from(length)];
            stream.read_exact(&mut buffer).await?;
            parse_message(&buffer)
        };
        tokio::time::timeout(self.timeout, tcp).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
    }
}

// ---- Messages ---------------------------------------------------------------------------------

const A: u16 = 1;
const NS: u16 = 2;
const CNAME: u16 = 5;
const SOA: u16 = 6;
const PTR: u16 = 12;
const MX: u16 = 15;
const TXT: u16 = 16;
const AAAA: u16 = 28;
const SRV: u16 = 33;
const NAPTR: u16 = 35;
const OPT: u16 = 41;
const TLSA: u16 = 52;
const ANY: u16 = 255;
const CAA: u16 = 257;

struct Record {
    kind: u16,
    ttl: u32,
    data: Data,
}

enum Data {
    Address(IpAddr),
    Name(String),
    Mx(u16, String),
    Txt(Vec<String>),
    Soa(SoaRecord),
    Srv(SrvRecord),
    Naptr(NaptrRecord),
    Caa(CaaRecord),
    Tlsa(TlsaRecord),
    Other,
}

struct Message {
    id: u16,
    truncated: bool,
    rcode: u8,
    answers: Vec<Record>,
}

fn random_id() -> u16 {
    let mut bytes = [0u8; 2];
    // SAFETY: `bytes` is a valid, writable two-byte buffer.
    unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), 2, 0) };
    u16::from_ne_bytes(bytes)
}

fn build_query(name: &str, kind: u16) -> Vec<u8> {
    let mut packet = Vec::with_capacity(64);
    packet.extend_from_slice(&random_id().to_be_bytes());
    // Recursion desired; one question, and one additional record for EDNS.
    packet.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 1]);
    for label in name.trim_end_matches('.').split('.').filter(|label| !label.is_empty()) {
        let label = &label.as_bytes()[..label.len().min(63)];
        packet.push(label.len() as u8);
        packet.extend_from_slice(label);
    }
    packet.push(0);
    packet.extend_from_slice(&kind.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    // OPT: root name, UDP payload 1232, no extended flags, no options.
    packet.extend_from_slice(&[0]);
    packet.extend_from_slice(&OPT.to_be_bytes());
    packet.extend_from_slice(&1232u16.to_be_bytes());
    packet.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    packet
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed DNS response")
}

struct Cursor<'a> {
    message: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn bytes(&mut self, length: usize) -> io::Result<&'a [u8]> {
        let end = self.at.checked_add(length).filter(|end| *end <= self.message.len()).ok_or_else(bad)?;
        let bytes = &self.message[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> io::Result<u32> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A possibly compressed name, without its trailing dot, as c-ares returns names.
    fn name(&mut self) -> io::Result<String> {
        let mut labels = Vec::new();
        let mut at = self.at;
        let mut resume = None;
        for _ in 0..128 {
            let length = *self.message.get(at).ok_or_else(bad)?;
            match length {
                0 => {
                    self.at = resume.unwrap_or(at + 1);
                    return Ok(labels.join("."));
                }
                length if length & 0xC0 == 0xC0 => {
                    let low = *self.message.get(at + 1).ok_or_else(bad)?;
                    resume.get_or_insert(at + 2);
                    at = (usize::from(length & 0x3F) << 8) | usize::from(low);
                }
                length => {
                    let start = at + 1;
                    let end = start + usize::from(length);
                    let label = self.message.get(start..end).ok_or_else(bad)?;
                    labels.push(String::from_utf8_lossy(label).into_owned());
                    at = end;
                }
            }
        }
        Err(bad())
    }

    fn character_string(&mut self) -> io::Result<String> {
        let length = usize::from(self.u8()?);
        Ok(String::from_utf8_lossy(self.bytes(length)?).into_owned())
    }
}

fn parse_data(cursor: &mut Cursor<'_>, kind: u16, length: usize) -> io::Result<Data> {
    let end = cursor.at + length;
    let data = match kind {
        A if length == 4 => {
            let bytes = cursor.bytes(4)?;
            Data::Address(IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])))
        }
        AAAA if length == 16 => {
            let bytes: [u8; 16] = cursor.bytes(16)?.try_into().map_err(|_| bad())?;
            Data::Address(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        NS | CNAME | PTR => Data::Name(cursor.name()?),
        MX => Data::Mx(cursor.u16()?, cursor.name()?),
        TXT => {
            let mut strings = Vec::new();
            while cursor.at < end {
                strings.push(cursor.character_string()?);
            }
            Data::Txt(strings)
        }
        SOA => Data::Soa(SoaRecord {
            nsname: cursor.name()?,
            hostmaster: cursor.name()?,
            serial: cursor.u32()?,
            refresh: cursor.u32()?,
            retry: cursor.u32()?,
            expire: cursor.u32()?,
            minttl: cursor.u32()?,
        }),
        SRV => Data::Srv(SrvRecord {
            priority: u32::from(cursor.u16()?),
            weight: u32::from(cursor.u16()?),
            port: cursor.u16()?,
            name: cursor.name()?,
        }),
        NAPTR => Data::Naptr(NaptrRecord {
            order: u32::from(cursor.u16()?),
            preference: u32::from(cursor.u16()?),
            flags: cursor.character_string()?,
            service: cursor.character_string()?,
            regexp: cursor.character_string()?,
            replacement: cursor.name()?,
        }),
        CAA => {
            let critical = u32::from(cursor.u8()?);
            let tag = cursor.character_string()?;
            let value = String::from_utf8_lossy(cursor.bytes(end.saturating_sub(cursor.at))?).into_owned();
            let mut record = CaaRecord {
                critical,
                issue: None,
                issuewild: None,
                iodef: None,
                contactemail: None,
                contactphone: None,
            };
            match tag.to_ascii_lowercase().as_str() {
                "issue" => record.issue = Some(value),
                "issuewild" => record.issuewild = Some(value),
                "iodef" => record.iodef = Some(value),
                "contactemail" => record.contactemail = Some(value),
                "contactphone" => record.contactphone = Some(value),
                _ => {}
            }
            Data::Caa(record)
        }
        TLSA if length >= 3 => Data::Tlsa(TlsaRecord {
            cert_usage: cursor.u8()?,
            selector: cursor.u8()?,
            match_: cursor.u8()?,
            data: cursor.bytes(length - 3)?.to_vec(),
        }),
        _ => Data::Other,
    };
    cursor.at = end;
    Ok(data)
}

fn parse_message(message: &[u8]) -> io::Result<Message> {
    let mut cursor = Cursor { message, at: 0 };
    let id = cursor.u16()?;
    let flags = cursor.u16()?;
    let questions = cursor.u16()?;
    let answers = cursor.u16()?;
    // NSCOUNT and ARCOUNT: authority and additional records are not read.
    cursor.bytes(4)?;
    for _ in 0..questions {
        cursor.name()?;
        cursor.bytes(4)?;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        cursor.name()?;
        let kind = cursor.u16()?;
        let _class = cursor.u16()?;
        let ttl = cursor.u32()?;
        let length = usize::from(cursor.u16()?);
        if cursor.at + length > message.len() {
            return Err(bad());
        }
        let data = parse_data(&mut cursor, kind, length)?;
        records.push(Record { kind, ttl, data });
    }
    Ok(Message { id, truncated: flags & 0x0200 != 0, rcode: (flags & 0x000F) as u8, answers: records })
}

fn syscall_for(kind: u16) -> &'static str {
    match kind {
        A => "queryA",
        AAAA => "queryAaaa",
        ANY => "queryAny",
        CAA => "queryCaa",
        CNAME => "queryCname",
        MX => "queryMx",
        NAPTR => "queryNaptr",
        NS => "queryNs",
        PTR => "queryPtr",
        SOA => "querySoa",
        SRV => "querySrv",
        TLSA => "queryTlsa",
        TXT => "queryTxt",
        _ => "query",
    }
}

/// Query `name` for `kind` and keep the answers of that kind, as c-ares's parsers do.
async fn resolve(config: Option<ResolverConfig>, name: &str, kind: u16, syscall: &str) -> Result<Vec<Record>, Error> {
    let resolver = Resolver::new(config)?;
    let message = resolver.query(name, kind).await.map_err(|code| ares(code, syscall, name))?;
    match message.rcode {
        0 => {}
        3 => return Err(ares("ENOTFOUND", syscall, name)),
        1 => return Err(ares("EFORMERR", syscall, name)),
        4 => return Err(ares("ENOTIMP", syscall, name)),
        _ => return Err(ares("EBADRESP", syscall, name)),
    }
    let records: Vec<_> =
        message.answers.into_iter().filter(|record| kind == ANY || record.kind == kind).collect();
    if records.is_empty() {
        return Err(ares("ENODATA", syscall, name));
    }
    Ok(records)
}

async fn resolve_kind(config: Option<ResolverConfig>, name: &str, kind: u16) -> Result<Vec<Record>, Error> {
    resolve(config, name, kind, syscall_for(kind)).await
}

fn names(records: Vec<Record>) -> Vec<String> {
    records.into_iter().filter_map(|record| if let Data::Name(name) = record.data { Some(name) } else { None }).collect()
}

/// The name a PTR query asks about for `ip`.
fn reverse_name(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            format!("{d}.{c}.{b}.{a}.in-addr.arpa")
        }
        IpAddr::V6(ip) => {
            let mut name = String::new();
            for byte in ip.octets().iter().rev() {
                name.push_str(&format!("{:x}.{:x}.", byte & 0xF, byte >> 4));
            }
            name.push_str("ip6.arpa");
            name
        }
    }
}

// ---- System resolver ---------------------------------------------------------------------------

fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    // SAFETY: getaddrinfo hands back addresses of the family it names.
    unsafe {
        match i32::from((*address).sa_family) {
            libc::AF_INET => {
                let address = &*address.cast::<libc::sockaddr_in>();
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr))))
            }
            libc::AF_INET6 => {
                let address = &*address.cast::<libc::sockaddr_in6>();
                Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

fn getaddrinfo(hostname: &str, family: Family, hints: u32) -> Result<Vec<IpAddr>, i32> {
    let host = CString::new(hostname).map_err(|_| libc::EAI_NONAME)?;
    // SAFETY: `addrinfo` is plain data, valid when zeroed.
    let mut request: libc::addrinfo = unsafe { std::mem::zeroed() };
    request.ai_family = match family {
        Family::Unspecified => libc::AF_UNSPEC,
        Family::Ipv4 => libc::AF_INET,
        Family::Ipv6 => libc::AF_INET6,
    };
    request.ai_socktype = libc::SOCK_STREAM;
    request.ai_flags = (hints & 0x38) as i32;
    let mut found = std::ptr::null_mut();
    // SAFETY: valid C string and hints; `found` is freed below.
    let status = unsafe { libc::getaddrinfo(host.as_ptr(), std::ptr::null(), &request, &mut found) };
    if status != 0 {
        return Err(status);
    }
    let mut addresses = Vec::new();
    let mut at = found;
    while !at.is_null() {
        // SAFETY: `at` walks the list getaddrinfo returned.
        let entry = unsafe { &*at };
        if let Some(ip) = sockaddr_ip(entry.ai_addr) {
            if !addresses.contains(&ip) {
                addresses.push(ip);
            }
        }
        at = entry.ai_next;
    }
    // SAFETY: `found` came from getaddrinfo and is freed once.
    unsafe { libc::freeaddrinfo(found) };
    Ok(addresses)
}

fn getnameinfo(address: SocketAddr) -> Result<(String, String), i32> {
    let mut host = [0 as libc::c_char; 1025];
    let mut service = [0 as libc::c_char; 32];
    let status = match address {
        SocketAddr::V4(v4) => {
            // SAFETY: plain data, valid when zeroed.
            let mut raw: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            raw.sin_port = v4.port().to_be();
            raw.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            // SAFETY: the address and buffers are valid for the given lengths.
            unsafe {
                libc::getnameinfo(
                    (&raw as *const libc::sockaddr_in).cast(),
                    size_of::<libc::sockaddr_in>() as u32,
                    host.as_mut_ptr(),
                    host.len() as u32,
                    service.as_mut_ptr(),
                    service.len() as u32,
                    libc::NI_NAMEREQD,
                )
            }
        }
        SocketAddr::V6(v6) => {
            // SAFETY: plain data, valid when zeroed.
            let mut raw: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = v6.port().to_be();
            raw.sin6_addr.s6_addr = v6.ip().octets();
            // SAFETY: the address and buffers are valid for the given lengths.
            unsafe {
                libc::getnameinfo(
                    (&raw as *const libc::sockaddr_in6).cast(),
                    size_of::<libc::sockaddr_in6>() as u32,
                    host.as_mut_ptr(),
                    host.len() as u32,
                    service.as_mut_ptr(),
                    service.len() as u32,
                    libc::NI_NAMEREQD,
                )
            }
        }
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: getnameinfo NUL-terminates both buffers on success.
    let text = |buffer: &[libc::c_char]| unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned();
    Ok((text(&host), text(&service)))
}

fn family_of(ip: IpAddr) -> Family {
    if ip.is_ipv4() { Family::Ipv4 } else { Family::Ipv6 }
}

// ---- Host -------------------------------------------------------------------------------------

impl Host for NodeView<'_> {
    async fn get_servers(&mut self) -> DnsResult<Vec<String>> {
        granted!(self);
        Ok(Ok(system_servers().into_iter().map(format_server).collect()))
    }

    async fn validate_servers(&mut self, servers: Vec<String>) -> DnsResult<Vec<String>> {
        granted!(self);
        Ok(servers
            .iter()
            .map(|server| parse_server(server).map(format_server).ok_or_else(|| invalid_ip(server)))
            .collect())
    }

    async fn lookup(&mut self, hostname: String, options: LookupOptions) -> DnsResult<Vec<LookupAddress>> {
        granted!(self);
        // Node answers an address literal without asking the resolver.
        if let Ok(ip) = hostname.parse::<IpAddr>() {
            return Ok(Ok(vec![LookupAddress { address: ip.to_string(), family: family_of(ip) }]));
        }
        let name = hostname.clone();
        let found = tokio::task::spawn_blocking(move || getaddrinfo(&name, options.family, options.hints)).await?;
        let mut addresses = match found {
            Ok(addresses) if !addresses.is_empty() => addresses,
            Ok(_) => return Ok(Err(system(libc::EAI_NONAME, "getaddrinfo", &hostname))),
            Err(status) => return Ok(Err(system(status, "getaddrinfo", &hostname))),
        };
        match options.order {
            ResultOrder::Ipv4First => addresses.sort_by_key(|ip| ip.is_ipv6()),
            ResultOrder::Ipv6First => addresses.sort_by_key(|ip| ip.is_ipv4()),
            ResultOrder::Verbatim => {}
        }
        if !options.all {
            addresses.truncate(1);
        }
        Ok(Ok(addresses.into_iter().map(|ip| LookupAddress { address: ip.to_string(), family: family_of(ip) }).collect()))
    }

    async fn lookup_service(&mut self, address: String, port: u16) -> DnsResult<LookupServiceResult> {
        granted!(self);
        let Ok(ip) = address.parse::<IpAddr>() else {
            return Ok(Err(Error {
                name: "TypeError".into(),
                message: format!("The argument 'address' is invalid. Received '{address}'"),
                code: Some("ERR_INVALID_ARG_VALUE".into()),
                errno: None,
                syscall: None,
                hostname: None,
            }));
        };
        let found = tokio::task::spawn_blocking(move || getnameinfo(SocketAddr::new(ip, port))).await?;
        Ok(match found {
            Ok((hostname, service)) => Ok(LookupServiceResult { hostname, service }),
            Err(status) => Err(system(status, "getnameinfo", &address)),
        })
    }

    async fn resolve4(&mut self, hostname: String, _ttl: bool, resolver: Option<ResolverConfig>) -> DnsResult<Vec<AddressWithTtl>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, A).await.map(addresses))
    }

    async fn resolve6(&mut self, hostname: String, _ttl: bool, resolver: Option<ResolverConfig>) -> DnsResult<Vec<AddressWithTtl>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, AAAA).await.map(addresses))
    }

    async fn resolve_any(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<AnyRecord>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, ANY).await.map(|records| {
            records
                .into_iter()
                .filter_map(|record| {
                    Some(match (record.kind, record.data) {
                        (A, Data::Address(ip)) => AnyRecord::A(AddressWithTtl { address: ip.to_string(), ttl: record.ttl }),
                        (AAAA, Data::Address(ip)) => {
                            AnyRecord::Aaaa(AddressWithTtl { address: ip.to_string(), ttl: record.ttl })
                        }
                        (CNAME, Data::Name(name)) => AnyRecord::Cname(name),
                        (NS, Data::Name(name)) => AnyRecord::Ns(name),
                        (PTR, Data::Name(name)) => AnyRecord::Ptr(name),
                        (_, Data::Mx(priority, exchange)) => AnyRecord::Mx(MxRecord { priority: u32::from(priority), exchange }),
                        (_, Data::Txt(entries)) => AnyRecord::Txt(entries),
                        (_, Data::Soa(soa)) => AnyRecord::Soa(soa),
                        (_, Data::Srv(srv)) => AnyRecord::Srv(srv),
                        (_, Data::Naptr(naptr)) => AnyRecord::Naptr(naptr),
                        (_, Data::Caa(caa)) => AnyRecord::Caa(caa),
                        (_, Data::Tlsa(tlsa)) => AnyRecord::Tlsa(tlsa),
                        _ => return None,
                    })
                })
                .collect()
        }))
    }

    async fn resolve_caa(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<CaaRecord>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, CAA).await.map(|records| {
            records.into_iter().filter_map(|record| if let Data::Caa(caa) = record.data { Some(caa) } else { None }).collect()
        }))
    }

    async fn resolve_cname(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<String>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, CNAME).await.map(names))
    }

    async fn resolve_mx(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<MxRecord>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, MX).await.map(|records| {
            records
                .into_iter()
                .filter_map(|record| match record.data {
                    Data::Mx(priority, exchange) => Some(MxRecord { priority: u32::from(priority), exchange }),
                    _ => None,
                })
                .collect()
        }))
    }

    async fn resolve_naptr(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<NaptrRecord>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, NAPTR).await.map(|records| {
            records.into_iter().filter_map(|record| if let Data::Naptr(naptr) = record.data { Some(naptr) } else { None }).collect()
        }))
    }

    async fn resolve_ns(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<String>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, NS).await.map(names))
    }

    async fn resolve_ptr(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<String>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, PTR).await.map(names))
    }

    async fn resolve_soa(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<SoaRecord> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, SOA).await.and_then(|records| {
            records
                .into_iter()
                .find_map(|record| if let Data::Soa(soa) = record.data { Some(soa) } else { None })
                .ok_or_else(|| ares("ENODATA", "querySoa", &hostname))
        }))
    }

    async fn resolve_srv(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<SrvRecord>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, SRV).await.map(|records| {
            records.into_iter().filter_map(|record| if let Data::Srv(srv) = record.data { Some(srv) } else { None }).collect()
        }))
    }

    async fn resolve_tlsa(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<TlsaRecord>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, TLSA).await.map(|records| {
            records.into_iter().filter_map(|record| if let Data::Tlsa(tlsa) = record.data { Some(tlsa) } else { None }).collect()
        }))
    }

    async fn resolve_txt(&mut self, hostname: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<Vec<String>>> {
        granted!(self);
        Ok(resolve_kind(resolver, &hostname, TXT).await.map(|records| {
            records.into_iter().filter_map(|record| if let Data::Txt(txt) = record.data { Some(txt) } else { None }).collect()
        }))
    }

    async fn reverse(&mut self, ip: String, resolver: Option<ResolverConfig>) -> DnsResult<Vec<String>> {
        granted!(self);
        let Ok(address) = ip.parse::<IpAddr>() else {
            return Ok(Err(ares("EINVAL", "getHostByAddr", &ip)));
        };
        Ok(resolve(resolver, &reverse_name(address), PTR, "getHostByAddr")
            .await
            .map(names)
            .map_err(|error| ares(error.code.as_deref().unwrap_or("ENOTFOUND"), "getHostByAddr", &ip)))
    }
}

fn addresses(records: Vec<Record>) -> Vec<AddressWithTtl> {
    records
        .into_iter()
        .filter_map(|record| match record.data {
            Data::Address(ip) => Some(AddressWithTtl { address: ip.to_string(), ttl: record.ttl }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_parse_and_print_like_node() {
        for (input, output) in [
            ("8.8.8.8", "8.8.8.8"),
            ("8.8.8.8:53", "8.8.8.8"),
            ("8.8.8.8:5353", "8.8.8.8:5353"),
            ("2001:4860:4860::8888", "2001:4860:4860::8888"),
            ("[2001:4860:4860::8888]:1053", "[2001:4860:4860::8888]:1053"),
        ] {
            assert_eq!(parse_server(input).map(format_server).as_deref(), Some(output), "{input}");
        }
        assert!(parse_server("not an address").is_none());
    }

    #[test]
    fn responses_with_compressed_names_parse() {
        let mut query = build_query("example.com", A);
        // Turn the query into its answer: QR, NOERROR, one answer, then the OPT record it sent
        // back in the additional section, as real servers do.
        query[2] = 0x81;
        query[3] = 0x80;
        query[7] = 1;
        let opt = query.split_off(query.len() - 11);
        query.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 0x0E, 0x10, 0, 4, 93, 184, 215, 14]);
        query.extend_from_slice(&opt);
        let message = parse_message(&query).unwrap();
        assert_eq!(message.answers.len(), 1);
        assert_eq!(message.answers[0].ttl, 3600);
        assert!(matches!(message.answers[0].data, Data::Address(ip) if ip.to_string() == "93.184.215.14"));
    }

    #[test]
    fn reverse_names() {
        assert_eq!(reverse_name("1.2.3.4".parse().unwrap()), "4.3.2.1.in-addr.arpa");
        assert!(reverse_name("::1".parse().unwrap()).starts_with("1.0.0.0."));
    }
}
