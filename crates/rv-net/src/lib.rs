//! User-mode networking, in the spirit of QEMU's "slirp": the guest sees a small virtual LAN
//! with a gateway, a DHCP server and a DNS server, and its connections leave through ordinary
//! host sockets, so no privileges or host network configuration are needed.
//!
//! - ARP, DHCP, DNS and UDP are handled directly.
//! - TCP connections are terminated by a smoltcp interface in "any IP" mode and spliced to host
//!   `TcpStream`s. The SYN-ACK is held back until the host connection is up, so the guest sees
//!   refused connections as refused.
//! - The gateway address reaches the host's loopback interface.
//!
//! Everything runs on one background thread around a `mio` event loop.

mod dhcp;
mod dns;
mod packet;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use mio::net::{TcpStream, UdpSocket};
use mio::{Events, Interest, Poll, Token, Waker};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, IpListenEndpoint};

pub use packet::Mac;
use packet::*;

/// Addresses of the virtual network. The defaults match QEMU's user networking.
#[derive(Clone, Debug)]
pub struct Config {
    pub guest_mac: Mac,
    pub gateway_mac: Mac,
    pub guest_ip: Ipv4Addr,
    pub gateway_ip: Ipv4Addr,
    pub dns_ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    /// Host TCP ports forwarded to guest ports: `(host, guest)`.
    pub tcp_forwards: Vec<(SocketAddr, u16)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            guest_mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            gateway_mac: [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02],
            guest_ip: Ipv4Addr::new(10, 0, 2, 15),
            gateway_ip: Ipv4Addr::new(10, 0, 2, 2),
            dns_ip: Ipv4Addr::new(10, 0, 2, 3),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            tcp_forwards: Vec::new(),
        }
    }
}

/// Receives frames destined for the guest.
pub type Sink = Box<dyn FnMut(Vec<u8>) + Send>;

/// Handle to the network thread.
pub struct UserNet {
    to_stack: Sender<Vec<u8>>,
    waker: Arc<Waker>,
}

impl UserNet {
    /// Starts the network. Frames for the guest are passed to `sink` on the network thread.
    pub fn spawn(config: Config, sink: Sink) -> io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKER)?);
        let (to_stack, from_guest) = mpsc::channel();
        let mut stack = Stack::new(config, poll, waker.clone(), from_guest, sink)?;
        std::thread::Builder::new().name("usernet".into()).spawn(move || stack.run())?;
        Ok(UserNet { to_stack, waker })
    }

    /// Sends a frame from the guest.
    pub fn send(&self, frame: Vec<u8>) {
        if self.to_stack.send(frame).is_ok() {
            let _ = self.waker.wake();
        }
    }
}

const WAKER: Token = Token(0);
const TCP_BUFFER: usize = 256 << 10;
const UDP_IDLE: Duration = Duration::from_secs(120);
const MAX_UDP_FLOWS: usize = 256;

struct TcpFlow {
    handle: SocketHandle,
    stream: TcpStream,
    key: (u16, SocketAddrV4),
    connected: bool,
    readable: bool,
    writable: bool,
    /// The host side has been shut down for writing (the guest sent FIN).
    shut_down: bool,
}

struct UdpFlow {
    socket: UdpSocket,
    key: (u16, SocketAddrV4),
    last_used: Instant,
}

/// Frames into and out of the smoltcp interface.
#[derive(Default)]
struct Queues {
    to_iface: VecDeque<Vec<u8>>,
    from_iface: VecDeque<Vec<u8>>,
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl phy::Device for Queues {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.to_iface.pop_front()?;
        Some((RxToken(frame), TxToken(&mut self.from_iface)))
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.from_iface))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

struct Stack {
    config: Config,
    poll: Poll,
    waker: Arc<Waker>,
    from_guest: Receiver<Vec<u8>>,
    sink: Sink,
    iface: Interface,
    queues: Queues,
    sockets: SocketSet<'static>,
    tcp: HashMap<Token, TcpFlow>,
    tcp_keys: HashMap<(u16, SocketAddrV4), Token>,
    udp: HashMap<Token, UdpFlow>,
    udp_keys: HashMap<(u16, SocketAddrV4), Token>,
    listeners: HashMap<Token, (mio::net::TcpListener, u16)>,
    /// Finished DNS lookups, as frames for the guest.
    dns_done: (Sender<Vec<u8>>, Receiver<Vec<u8>>),
    next_token: usize,
    epoch: Instant,
    /// Guest-side source ports for forwarded connections.
    next_forward_port: u16,
}

impl Stack {
    fn new(
        config: Config,
        poll: Poll,
        waker: Arc<Waker>,
        from_guest: Receiver<Vec<u8>>,
        sink: Sink,
    ) -> io::Result<Self> {
        let mut queues = Queues::default();
        let epoch = Instant::now();
        let mut iface_config =
            IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(config.gateway_mac)));
        iface_config.random_seed = epoch.elapsed().as_nanos() as u64 ^ 0x5eed;
        let mut iface = Interface::new(iface_config, &mut queues, smoltcp::time::Instant::ZERO);
        let prefix = u32::from(config.netmask).count_ones() as u8;
        iface.update_ip_addrs(|addrs| {
            for ip in [config.gateway_ip, config.dns_ip] {
                addrs.push(IpCidr::new(ip.into(), prefix)).unwrap();
            }
        });
        iface.set_any_ip(true);
        let mut stack = Stack {
            config,
            poll,
            waker,
            from_guest,
            sink,
            iface,
            queues,
            sockets: SocketSet::new(Vec::new()),
            tcp: HashMap::new(),
            tcp_keys: HashMap::new(),
            udp: HashMap::new(),
            udp_keys: HashMap::new(),
            listeners: HashMap::new(),
            dns_done: mpsc::channel(),
            next_token: 1,
            epoch,
            next_forward_port: 40000,
        };
        for (host, guest_port) in stack.config.tcp_forwards.clone() {
            let mut listener = mio::net::TcpListener::bind(host)?;
            let token = stack.token();
            stack.poll.registry().register(&mut listener, token, Interest::READABLE)?;
            stack.listeners.insert(token, (listener, guest_port));
        }
        Ok(stack)
    }

    fn token(&mut self) -> Token {
        self.next_token += 1;
        Token(self.next_token)
    }

    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.epoch.elapsed().as_micros() as i64)
    }

    /// Where a guest destination is reached from the host.
    fn host_addr(&self, dst: SocketAddrV4) -> SocketAddrV4 {
        if *dst.ip() == self.config.gateway_ip {
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, dst.port())
        } else {
            dst
        }
    }

    fn run(&mut self) {
        let mut events = Events::with_capacity(256);
        let mut last_expiry = Instant::now();
        loop {
            let timeout = self
                .iface
                .poll_delay(self.now(), &self.sockets)
                .map(|d| Duration::from_micros(d.total_micros()))
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(1));
            if let Err(e) = self.poll.poll(&mut events, Some(timeout))
                && e.kind() != io::ErrorKind::Interrupted
            {
                eprintln!("usernet: poll failed: {e}\r");
                return;
            }
            for event in events.iter() {
                let token = event.token();
                if let Some(flow) = self.tcp.get_mut(&token) {
                    flow.readable |= event.is_readable() || event.is_read_closed() || event.is_error();
                    flow.writable |= event.is_writable() || event.is_write_closed() || event.is_error();
                } else if self.udp.contains_key(&token) {
                    self.udp_receive(token);
                } else if self.listeners.contains_key(&token) {
                    self.accept_forwarded(token);
                }
            }
            let mut guest_frames = Vec::new();
            while let Ok(frame) = self.from_guest.try_recv() {
                guest_frames.push(frame);
            }
            for frame in guest_frames {
                self.handle_guest_frame(frame);
            }
            while let Ok(frame) = self.dns_done.1.try_recv() {
                (self.sink)(frame);
            }
            // Splice, let the interface act, then splice again what it made possible.
            for _ in 0..2 {
                self.splice_tcp();
                let now = self.now();
                self.iface.poll(now, &mut self.queues, &mut self.sockets);
                while let Some(frame) = self.queues.from_iface.pop_front() {
                    if std::env::var_os("RV_NET_TRACE").is_some() {
                        eprintln!("iface> {}\r", summary(&frame));
                    }
                    (self.sink)(frame);
                }
            }
            self.reap_tcp();
            if last_expiry.elapsed() >= Duration::from_secs(1) {
                last_expiry = Instant::now();
                self.expire_udp();
            }
        }
    }

    fn handle_guest_frame(&mut self, frame: Vec<u8>) {
        if std::env::var_os("RV_NET_TRACE").is_some() {
            eprintln!("guest> {}\r", summary(&frame));
        }
        let Some(eth) = parse_ethernet(&frame) else { return };
        match eth.ethertype {
            ETHERTYPE_ARP => {
                let Some(arp) = parse_arp(eth.payload) else { return };
                // smoltcp answers for every address in "any IP" mode, so keep probes for the
                // guest's own address (and other non-guest senders) away from it.
                if arp.target_ip != self.config.guest_ip && arp.sender_ip == self.config.guest_ip {
                    self.queues.to_iface.push_back(frame);
                }
            }
            ETHERTYPE_IPV4 => {
                let Some(ip) = parse_ipv4(eth.payload) else { return };
                if ip.src != self.config.guest_ip && ip.protocol != PROTO_UDP {
                    return;
                }
                match ip.protocol {
                    PROTO_UDP => self.guest_udp(&eth, &ip),
                    PROTO_TCP => {
                        self.guest_tcp(&ip);
                        self.queues.to_iface.push_back(frame);
                    }
                    // Only the virtual hosts answer pings; other ICMP is not routed.
                    _ if ip.dst == self.config.gateway_ip || ip.dst == self.config.dns_ip => {
                        self.queues.to_iface.push_back(frame)
                    }
                    _ => {}
                }
            }
            // IPv6 is not routed.
            _ => {}
        }
    }

    fn guest_udp(&mut self, eth: &Ethernet, ip: &Ipv4) {
        let Some(udp) = parse_udp(ip.payload) else { return };
        let c = &self.config;
        if udp.dst_port == 67 {
            if let Some(reply) = dhcp::handle(c, udp.payload) {
                let frame =
                    udp_frame(c.gateway_mac, eth.src, (c.gateway_ip, 67), (Ipv4Addr::BROADCAST, 68), &reply);
                (self.sink)(frame);
            }
            return;
        }
        if ip.src != c.guest_ip {
            return;
        }
        let dst = SocketAddrV4::new(ip.dst, udp.dst_port);
        if ip.dst == c.dns_ip && udp.dst_port == 53 {
            self.dns_query(udp.src_port, udp.payload);
            return;
        }
        if ip.dst.is_broadcast() || ip.dst.is_multicast() || ip.dst == broadcast(c) {
            return;
        }
        let key = (udp.src_port, dst);
        let token = match self.udp_keys.get(&key) {
            Some(&t) => t,
            None => {
                if self.udp.len() >= MAX_UDP_FLOWS {
                    self.expire_oldest_udp();
                }
                let Ok(mut socket) = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))) else {
                    return;
                };
                let token = self.token();
                if self.poll.registry().register(&mut socket, token, Interest::READABLE).is_err() {
                    return;
                }
                self.udp.insert(token, UdpFlow { socket, key, last_used: Instant::now() });
                self.udp_keys.insert(key, token);
                token
            }
        };
        let target = self.host_addr(dst);
        let flow = self.udp.get_mut(&token).unwrap();
        flow.last_used = Instant::now();
        let _ = flow.socket.send_to(udp.payload, target.into());
    }

    fn dns_query(&mut self, guest_port: u16, payload: &[u8]) {
        let c = self.config.clone();
        let reply_frame = move |reply: &[u8]| {
            udp_frame(c.gateway_mac, c.guest_mac, (c.dns_ip, 53), (c.guest_ip, guest_port), reply)
        };
        match dns::parse(payload) {
            Some(query) => {
                let done = self.dns_done.0.clone();
                let waker = self.waker.clone();
                // getaddrinfo blocks, so each lookup gets a thread.
                let _ = std::thread::Builder::new().name("usernet-dns".into()).spawn(move || {
                    let reply = dns::resolve(&query);
                    if done.send(reply_frame(&reply)).is_ok() {
                        let _ = waker.wake();
                    }
                });
            }
            None => {
                if let Some(reply) = dns::format_error(payload) {
                    (self.sink)(reply_frame(&reply));
                }
            }
        }
    }

    fn udp_receive(&mut self, token: Token) {
        let Some(flow) = self.udp.get_mut(&token) else { return };
        let mut buf = vec![0u8; 65536];
        loop {
            match flow.socket.recv_from(&mut buf) {
                Ok((n, SocketAddr::V4(from))) => {
                    flow.last_used = Instant::now();
                    let (guest_port, dst) = flow.key;
                    let c = &self.config;
                    // Replies from the host's loopback appear to come from the gateway.
                    let src = if *from.ip() == Ipv4Addr::LOCALHOST && *dst.ip() == c.gateway_ip {
                        dst
                    } else {
                        from
                    };
                    if n + IP_HEADER + UDP_HEADER > 65535 {
                        continue;
                    }
                    let frame = udp_frame(
                        c.gateway_mac,
                        c.guest_mac,
                        (*src.ip(), src.port()),
                        (c.guest_ip, guest_port),
                        &buf[..n],
                    );
                    (self.sink)(frame);
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
        }
    }

    fn remove_udp(&mut self, token: Token) {
        if let Some(mut flow) = self.udp.remove(&token) {
            let _ = self.poll.registry().deregister(&mut flow.socket);
            self.udp_keys.remove(&flow.key);
        }
    }

    fn expire_udp(&mut self) {
        let old: Vec<Token> =
            self.udp.iter().filter(|(_, f)| f.last_used.elapsed() > UDP_IDLE).map(|(&t, _)| t).collect();
        for t in old {
            self.remove_udp(t);
        }
    }

    fn expire_oldest_udp(&mut self) {
        if let Some(t) = self.udp.iter().min_by_key(|(_, f)| f.last_used).map(|(&t, _)| t) {
            self.remove_udp(t);
        }
    }

    /// Opens a host connection for a guest SYN to a new destination.
    fn guest_tcp(&mut self, ip: &Ipv4) {
        let Some(tcp) = parse_tcp_header(ip.payload) else { return };
        if !tcp.syn || tcp.ack || tcp.rst {
            return;
        }
        let dst = SocketAddrV4::new(ip.dst, tcp.dst_port);
        let key = (tcp.src_port, dst);
        if self.tcp_keys.contains_key(&key) || ip.dst == self.config.dns_ip {
            return;
        }
        // On failure there is no socket, and the interface answers the SYN with a reset.
        let Ok(stream) = TcpStream::connect(self.host_addr(dst).into()) else { return };
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
        );
        socket.pause_synack(true);
        socket.set_nagle_enabled(false);
        if socket.listen(IpListenEndpoint { addr: Some(ip.dst.into()), port: tcp.dst_port }).is_err() {
            return;
        }
        self.add_tcp_flow(socket, stream, key, false);
    }

    fn add_tcp_flow(
        &mut self,
        socket: tcp::Socket<'static>,
        mut stream: TcpStream,
        key: (u16, SocketAddrV4),
        connected: bool,
    ) {
        let token = self.token();
        if self.poll.registry().register(&mut stream, token, Interest::READABLE | Interest::WRITABLE).is_err()
        {
            return;
        }
        let handle = self.sockets.add(socket);
        // Connected streams may already have data; reading finds out.
        self.tcp.insert(
            token,
            TcpFlow {
                handle,
                stream,
                key,
                connected,
                readable: connected,
                writable: connected,
                shut_down: false,
            },
        );
        self.tcp_keys.insert(key, token);
    }

    /// A host connection to a forwarded port: connect to the guest from the gateway.
    fn accept_forwarded(&mut self, token: Token) {
        loop {
            let (listener, guest_port) = self.listeners.get_mut(&token).unwrap();
            let guest_port = *guest_port;
            let Ok((stream, _)) = listener.accept() else { return };
            self.next_forward_port = self.next_forward_port.checked_add(1).unwrap_or(40000);
            let local = SocketAddrV4::new(self.config.gateway_ip, self.next_forward_port);
            let mut socket = tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
                tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
            );
            socket.set_nagle_enabled(false);
            let remote = SocketAddrV4::new(self.config.guest_ip, guest_port);
            let cx = self.iface.context();
            if socket.connect(cx, (*remote.ip(), remote.port()), (*local.ip(), local.port())).is_err() {
                continue;
            }
            // Keyed by the guest-side tuple as seen from the guest: its port and our address.
            self.add_tcp_flow(socket, stream, (guest_port, local), true);
        }
    }

    fn splice_tcp(&mut self) {
        for flow in self.tcp.values_mut() {
            let socket = self.sockets.get_mut::<tcp::Socket>(flow.handle);
            if !flow.connected {
                if !flow.writable {
                    continue;
                }
                match flow.stream.take_error() {
                    Ok(None) if flow.stream.peer_addr().is_ok() => {
                        flow.connected = true;
                        socket.pause_synack(false);
                    }
                    Ok(None) => {
                        flow.writable = false;
                        continue;
                    }
                    _ => {
                        socket.abort();
                        continue;
                    }
                }
            }
            // Host to guest.
            while flow.readable && socket.can_send() {
                let mut eof = false;
                let mut failed = false;
                let r = socket.send(|buf| match flow.stream.read(buf) {
                    Ok(0) => {
                        eof = true;
                        (0, ())
                    }
                    Ok(n) => (n, ()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        flow.readable = false;
                        (0, ())
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => (0, ()),
                    Err(_) => {
                        failed = true;
                        (0, ())
                    }
                });
                if r.is_err() || failed {
                    socket.abort();
                    break;
                }
                if eof {
                    flow.readable = false;
                    socket.close();
                    break;
                }
            }
            // Guest to host.
            while flow.writable && socket.can_recv() {
                let mut failed = false;
                let r = socket.recv(|buf| match flow.stream.write(buf) {
                    Ok(n) => (n, n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        flow.writable = false;
                        (0, 0)
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => (0, 1),
                    Err(_) => {
                        failed = true;
                        (0, 0)
                    }
                });
                if failed || r.is_err() {
                    socket.abort();
                    break;
                }
                if r == Ok(0) {
                    break;
                }
            }
            // The guest sent FIN and everything before it has been passed on.
            let guest_done = matches!(
                socket.state(),
                tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait
            );
            if !flow.shut_down && guest_done && socket.recv_queue() == 0 {
                flow.shut_down = true;
                let _ = flow.stream.shutdown(Shutdown::Write);
            }
        }
    }

    fn reap_tcp(&mut self) {
        let done: Vec<Token> = self
            .tcp
            .iter()
            .filter(|(_, f)| {
                matches!(
                    self.sockets.get::<tcp::Socket>(f.handle).state(),
                    tcp::State::Closed | tcp::State::TimeWait
                )
            })
            .map(|(&t, _)| t)
            .collect();
        for t in done {
            let mut flow = self.tcp.remove(&t).unwrap();
            let _ = self.poll.registry().deregister(&mut flow.stream);
            self.sockets.remove(flow.handle);
            self.tcp_keys.remove(&flow.key);
        }
    }
}

fn broadcast(c: &Config) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(c.gateway_ip) | !u32::from(c.netmask))
}

fn summary(frame: &[u8]) -> String {
    let Some(eth) = parse_ethernet(frame) else { return "runt".into() };
    match eth.ethertype {
        ETHERTYPE_ARP => match parse_arp(eth.payload) {
            Some(a) => format!("arp {} -> {}", a.sender_ip, a.target_ip),
            None => "arp?".into(),
        },
        ETHERTYPE_IPV4 => match parse_ipv4(eth.payload) {
            Some(ip) if ip.protocol == PROTO_TCP => {
                let p = ip.payload;
                let flags = p.get(13).copied().unwrap_or(0);
                format!(
                    "tcp {}:{} -> {}:{} flags {flags:#04x} len {}",
                    ip.src,
                    u16::from_be_bytes([p[0], p[1]]),
                    ip.dst,
                    u16::from_be_bytes([p[2], p[3]]),
                    p.len()
                )
            }
            Some(ip) => format!("ip proto {} {} -> {} len {}", ip.protocol, ip.src, ip.dst, ip.payload.len()),
            None => "ipv4?".into(),
        },
        t => format!("ethertype {t:#06x}"),
    }
}
