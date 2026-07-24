//! User-mode network for the guest's virtio-net device.
//!
//! The emulated NIC hands whole ethernet frames to a `NetBackend` (see
//! emu/src/net.rs); this module is the other end of the cable. A smoltcp
//! `Interface` terminates the guest's ARP/IPv4 traffic at 10.0.2.2 (the
//! "gateway"), a ~150-line DHCP server leases 10.0.2.15 to the guest, and
//! inbound TCP port-forwards splice real `wasi:sockets` connections (e.g. the
//! deployment's `tcp:2222` ingress) onto smoltcp connections into the guest
//! (e.g. an sshd on guest port 22).
//!
//! Outbound, the gateway plays slirp (QEMU's user-mode NAT). wasip2 has no
//! raw sockets, so nothing is bridged — each guest flow is re-terminated on a
//! real socket the platform's egress can carry:
//!
//!  - guest TCP SYN to an external ip:port → a real `TcpStream::connect`,
//!    then the same splice machinery as the inbound forwards (the smoltcp
//!    side is a listener on that exact ip:port, accepted via `any_ip`);
//!  - guest UDP to an external ip:port → a real `UdpSocket` per flow, reply
//!    datagrams re-framed to the guest with the external source;
//!  - DNS at 10.0.2.2:53 (what the DHCP lease advertises) → answered with
//!    the platform's own name lookup (`ToSocketAddrs`), A records only;
//!  - ICMP echo ("ping 8.8.8.8") → answered by the gateway itself, exactly
//!    like slirp: a reply proves the NAT is up, not that the target answered
//!    a real ICMP packet (none can be sent from inside an enclave).
//!
//! `net.outbound: false` in the config removes all of this and restores the
//! sealed posture (the machine cannot exfiltrate anything by itself).
//!
//!   browser/ssh ──tcp:2222──► app listener ──splice──► smoltcp ──frames──►
//!   virtio-net ──► guest kernel ──► sshd :22
//!   guest curl/ping ──frames──► smoltcp/NAT ──splice──► real socket ──► world

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use riscv_emu_rust::net::NetBackend;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint,
    Ipv4Address,
};

pub const HOST_IP: [u8; 4] = [10, 0, 2, 2];
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const HOST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0x02];
const DHCP_LEASE_SECS: u32 = 7 * 24 * 3600;
const FRAME_QUEUE_CAP: usize = 256; // frames buffered each way before dropping
const SPLICE_BUF: usize = 64 * 1024;

// ---- outbound NAT limits ----------------------------------------------------
// The real connect() is blocking (single thread, no async connect on wasip2),
// so it is bounded tightly: a guest dialing a dead IP stalls the machine for
// at most this long, once, and then gets an RST.
const NAT_CONNECT_TIMEOUT: Duration = Duration::from_millis(2500);
const NAT_TCP_MAX: usize = 32; // concurrent outbound TCP splices
const NAT_UDP_MAX: usize = 64; // concurrent outbound UDP flows
const NAT_UDP_IDLE: Duration = Duration::from_secs(60); // flow expiry
// A NAT listener whose guest never completes the TCP handshake would leak the
// real connection; reap it after this long in LISTEN.
const NAT_HANDSHAKE_GRACE: Duration = Duration::from_secs(20);
const UDP_MAX_PAYLOAD: usize = 1472; // MTU 1514 - 14 eth - 20 ip - 8 udp
const DNS_TTL_SECS: u32 = 60;
const DNS_MAX_ANSWERS: usize = 8;

fn host_ip() -> Ipv4Address {
    Ipv4Address::new(HOST_IP[0], HOST_IP[1], HOST_IP[2], HOST_IP[3])
}

fn guest_ip() -> Ipv4Address {
    Ipv4Address::new(GUEST_IP[0], GUEST_IP[1], GUEST_IP[2], GUEST_IP[3])
}

// ---- the emulator-facing backend: two bounded frame queues -----------------

/// Owned by the virtio-net device inside the emulator; the app reaches it via
/// `Emulator::get_mut_net_backend()`-style accessors each loop turn.
pub struct HostNet {
    to_guest: VecDeque<Vec<u8>>,
    from_guest: VecDeque<Vec<u8>>,
}

impl HostNet {
    pub fn new() -> Self {
        HostNet { to_guest: VecDeque::new(), from_guest: VecDeque::new() }
    }
}

impl NetBackend for HostNet {
    fn guest_tx(&mut self, frame: Vec<u8>) {
        if self.from_guest.len() < FRAME_QUEUE_CAP {
            self.from_guest.push_back(frame);
        } // else: drop, like a real NIC with a full ring
    }
    fn guest_rx(&mut self) -> Option<Vec<u8>> {
        self.to_guest.pop_front()
    }
    fn host_push(&mut self, frame: Vec<u8>) {
        if self.to_guest.len() < FRAME_QUEUE_CAP {
            self.to_guest.push_back(frame);
        }
    }
    fn host_pop(&mut self) -> Option<Vec<u8>> {
        self.from_guest.pop_front()
    }
}

// ---- smoltcp phy over in-memory frame queues -------------------------------

struct QueueDevice {
    rx: VecDeque<Vec<u8>>, // frames from the guest, into smoltcp
    tx: VecDeque<Vec<u8>>, // frames from smoltcp, toward the guest
}

struct QueueRxToken(Vec<u8>);
struct QueueTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl smoltcp::phy::RxToken for QueueRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

impl<'a> smoltcp::phy::TxToken for QueueTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        if self.0.len() < FRAME_QUEUE_CAP {
            self.0.push_back(buf);
        }
        r
    }
}

impl Device for QueueDevice {
    type RxToken<'a> = QueueRxToken;
    type TxToken<'a> = QueueTxToken<'a>;

    fn receive(&mut self, _t: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((QueueRxToken(frame), QueueTxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _t: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(QueueTxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps
    }
}

// ---- inbound port-forwards -------------------------------------------------

#[derive(Clone, Copy)]
pub struct ForwardCfg {
    pub listen: u16, // logical port on the deployment (tcp:<listen> in ENCLAVE_PORTS)
    pub to: u16,     // port inside the guest
}

struct Listener {
    listener: TcpListener,
    cfg: ForwardCfg,
}

/// One spliced TCP connection: a real socket on one side, a smoltcp socket
/// toward the guest on the other. Inbound forwards and outbound NAT flows are
/// the same thing once established — only how they were set up differs.
struct Splice {
    stream: TcpStream,
    handle: SocketHandle,
    desc: String, // for logs: "tcp:2222 -> guest:22" or "nat 1.2.3.4:443"
    // Outbound flows carry their (guest port, dst ip, dst port) tuple so a
    // retransmitted SYN doesn't open a second real connection.
    nat: Option<(u16, [u8; 4], u16)>,
    created: Instant,
    outbuf: VecDeque<u8>, // guest → real socket, pending write
    stream_eof: bool,
    eof_since: Option<Instant>, // grace timer once the real side is done
}

/// One outbound UDP flow: guest ip:port ⇄ external ip:port over a real
/// non-blocking `UdpSocket`, expired after `NAT_UDP_IDLE` without traffic.
struct UdpNat {
    sock: UdpSocket,
    guest_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    guest_mac: [u8; 6], // for re-framing replies
    last_used: Instant,
}

// A guest process that never closes its end (e.g. a lingering CLOSE_WAIT)
// would pin the splice forever after the real client has gone; reap it once
// the client has been gone this long with nothing left to flush.
const HALF_CLOSE_GRACE: Duration = Duration::from_secs(30);

/// What `classify` decided about one guest frame.
#[derive(PartialEq)]
enum Verdict {
    Pass, // hand it to smoltcp (gateway traffic, DHCP, TCP flows)
    Drop, // consumed here (NAT'd, answered, or unroutable)
}

// ---- the stack -------------------------------------------------------------

pub struct NetStack {
    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    dns: Option<SocketHandle>, // 10.0.2.2:53, outbound only
    listeners: Vec<Listener>,
    splices: Vec<Splice>,
    udp_nat: Vec<UdpNat>,
    outbound: bool,
    ephemeral: u16,
    epoch: std::time::Instant,
    pub rx_frames: u64, // guest → host
    pub tx_frames: u64, // host → guest
}

/// Resolves `tcp:<logical>=<actual>` from ENCLAVE_PORTS, defaulting to the
/// logical port itself for local runs without the platform env.
fn resolve_tcp_port(logical: u16) -> u16 {
    let Ok(spec) = std::env::var("ENCLAVE_PORTS") else {
        return logical;
    };
    for entry in spec.split(',') {
        if let Some(rest) = entry.trim().strip_prefix("tcp:") {
            if let Some((l, a)) = rest.split_once('=') {
                if l.parse() == Ok(logical) {
                    if let Ok(actual) = a.parse() {
                        return actual;
                    }
                }
            }
        }
    }
    logical
}

impl NetStack {
    pub fn new(forwards: &[ForwardCfg], outbound: bool) -> Self {
        let mut device = QueueDevice { rx: VecDeque::new(), tx: VecDeque::new() };
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(HOST_MAC)));
        let now = SmolInstant::from_millis(0);
        let mut iface = Interface::new(config, &mut device, now);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(host_ip()), 24))
                .expect("one address fits");
        });
        if outbound {
            // NAT listeners sit on foreign addresses (e.g. 1.2.3.4:443). For
            // smoltcp to process a packet not addressed to it, any_ip must be
            // on AND a route for the destination must resolve to one of the
            // interface's own addresses — hence a default route via ourselves.
            iface.set_any_ip(true);
            iface
                .routes_mut()
                .add_default_ipv4_route(host_ip())
                .expect("fresh route table has room");
        }

        let mut sockets = SocketSet::new(vec![]);
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 4096]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 4096]);
        let mut dhcp_socket = udp::Socket::new(rx, tx);
        dhcp_socket.bind(67).expect("bind dhcp");
        let dhcp = sockets.add(dhcp_socket);

        let dns = outbound.then(|| {
            let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 4096]);
            let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0; 8192]);
            let mut dns_socket = udp::Socket::new(rx, tx);
            dns_socket.bind(53).expect("bind dns");
            sockets.add(dns_socket)
        });

        let mut listeners = Vec::new();
        for cfg in forwards {
            let actual = resolve_tcp_port(cfg.listen);
            match TcpListener::bind(("127.0.0.1", actual)) {
                Ok(l) => {
                    if l.set_nonblocking(true).is_ok() {
                        eprintln!(
                            "[risc-box] forward: tcp:{} (bound {actual}) -> guest {}.{}.{}.{}:{}",
                            cfg.listen, GUEST_IP[0], GUEST_IP[1], GUEST_IP[2], GUEST_IP[3], cfg.to
                        );
                        listeners.push(Listener { listener: l, cfg: *cfg });
                    }
                }
                Err(e) => eprintln!("[risc-box] forward tcp:{} bind {actual} failed: {e}", cfg.listen),
            }
        }

        NetStack {
            device,
            iface,
            sockets,
            dhcp,
            dns,
            listeners,
            splices: Vec::new(),
            udp_nat: Vec::new(),
            outbound,
            ephemeral: 49152,
            epoch: std::time::Instant::now(),
            rx_frames: 0,
            tx_frames: 0,
        }
    }

    pub fn forwards(&self) -> Vec<ForwardCfg> {
        self.listeners.iter().map(|l| l.cfg).collect()
    }

    pub fn active_splices(&self) -> usize {
        self.splices.len()
    }

    pub fn outbound_enabled(&self) -> bool {
        self.outbound
    }

    pub fn nat_tcp_flows(&self) -> usize {
        self.splices.iter().filter(|s| s.nat.is_some()).count()
    }

    pub fn nat_udp_flows(&self) -> usize {
        self.udp_nat.len()
    }

    /// One turn of the network: exchange frames with the emulator's backend,
    /// run smoltcp, serve DHCP/DNS, accept and shuttle forwards, pump the
    /// NAT. Returns true if anything moved (the caller uses it to keep the
    /// guest un-throttled while traffic is in flight).
    pub fn pump(&mut self, backend: &mut dyn NetBackend) -> bool {
        let mut activity = false;

        while let Some(frame) = backend.host_pop() {
            self.rx_frames += 1;
            activity = true;
            // The classifier peels off what smoltcp cannot serve (external
            // ICMP/UDP), sets up NAT for external TCP, and passes the rest.
            if self.classify(&frame) == Verdict::Pass && self.device.rx.len() < FRAME_QUEUE_CAP {
                self.device.rx.push_back(frame);
            }
        }

        self.accept_new();

        let now = SmolInstant::from_millis(self.epoch.elapsed().as_millis() as i64);
        self.iface.poll(now, &mut self.device, &mut self.sockets);

        self.serve_dhcp();
        self.serve_dns();
        if self.shuttle() {
            activity = true;
        }
        if self.poll_udp_nat() {
            activity = true;
        }
        // shuttling queues data in sockets; poll again so it egresses this turn
        self.iface.poll(now, &mut self.device, &mut self.sockets);

        while let Some(frame) = self.device.tx.pop_front() {
            self.tx_frames += 1;
            activity = true;
            backend.host_push(frame);
        }
        activity
    }

    /// Queues a synthesized frame for delivery into the guest (it leaves with
    /// the same end-of-pump drain as smoltcp's own output).
    fn push_to_guest(&mut self, frame: Vec<u8>) {
        if self.device.tx.len() < FRAME_QUEUE_CAP {
            self.device.tx.push_back(frame);
        }
    }

    /// Looks at one guest frame and decides its path. Everything stays `Pass`
    /// (straight into smoltcp) unless outbound NAT is on and the destination
    /// is beyond the 10.0.2.0/24 wire.
    fn classify(&mut self, f: &[u8]) -> Verdict {
        if !self.outbound || f.len() < 34 || f[12] != 0x08 || f[13] != 0x00 {
            return Verdict::Pass; // not IPv4 (ARP etc.), or sealed mode
        }
        let ihl = ((f[14] & 0x0f) as usize) * 4;
        if (f[14] >> 4) != 4 || ihl < 20 || f.len() < 14 + ihl + 4 {
            return Verdict::Pass; // malformed; let smoltcp shrug at it
        }
        let dst: [u8; 4] = f[30..34].try_into().unwrap();
        if dst == HOST_IP || dst == [255, 255, 255, 255] {
            return Verdict::Pass; // the gateway itself, or DHCP broadcast
        }
        if dst[0] & 0xf0 == 0xe0 || dst == [GUEST_IP[0], GUEST_IP[1], GUEST_IP[2], 255] {
            return Verdict::Drop; // multicast noise (mDNS/SSDP), subnet broadcast
        }
        let on_subnet = dst[0] == HOST_IP[0] && dst[1] == HOST_IP[1] && dst[2] == HOST_IP[2];
        let l4 = 14 + ihl;
        match f[23] {
            // ICMP: echo requests to external hosts are answered here, like
            // slirp — no ICMP can leave a wasi sandbox. On-subnet ghosts stay
            // silent (nothing lives there), everything else is dropped.
            1 => {
                if !on_subnet && f.len() >= l4 + 8 && f[l4] == 8 && f[l4 + 1] == 0 {
                    let total = u16::from_be_bytes([f[16], f[17]]) as usize;
                    let end = (14 + total).min(f.len());
                    if end >= l4 + 8 {
                        let reply = icmp_echo_reply(f, ihl, end);
                        self.push_to_guest(reply);
                    }
                }
                Verdict::Drop
            }
            // TCP: a fresh SYN to an external endpoint opens the real
            // connection and its NAT listener, then the frame proceeds into
            // smoltcp, which accepts it (or RSTs it if the connect failed —
            // no listener). Established-flow segments just pass.
            6 => {
                if !on_subnet && f.len() >= l4 + 14 && f[l4 + 13] & 0x12 == 0x02 {
                    let sport = u16::from_be_bytes([f[l4], f[l4 + 1]]);
                    let dport = u16::from_be_bytes([f[l4 + 2], f[l4 + 3]]);
                    self.ensure_tcp_nat(sport, dst, dport);
                }
                Verdict::Pass
            }
            // UDP: external datagrams ride a real socket per flow.
            17 => {
                if !on_subnet && f.len() >= l4 + 8 {
                    let sport = u16::from_be_bytes([f[l4], f[l4 + 1]]);
                    let dport = u16::from_be_bytes([f[l4 + 2], f[l4 + 3]]);
                    let udp_len = u16::from_be_bytes([f[l4 + 4], f[l4 + 5]]) as usize;
                    if udp_len >= 8 {
                        let end = (l4 + udp_len).min(f.len());
                        let mac: [u8; 6] = f[6..12].try_into().unwrap();
                        self.udp_nat_out(mac, sport, dst, dport, &f[l4 + 8..end]);
                    }
                }
                Verdict::Drop
            }
            _ => Verdict::Drop, // no NAT for other protocols
        }
    }

    /// Opens the real connection + smoltcp listener for a guest SYN to an
    /// external endpoint, unless the flow already exists. The connect is the
    /// one blocking step in the NAT (see NAT_CONNECT_TIMEOUT).
    fn ensure_tcp_nat(&mut self, sport: u16, dst: [u8; 4], dport: u16) {
        if dport == 0 || self.splices.iter().any(|s| s.nat == Some((sport, dst, dport))) {
            return; // SYN retransmission for a flow being set up
        }
        let ip = Ipv4Addr::new(dst[0], dst[1], dst[2], dst[3]);
        if self.nat_tcp_flows() >= NAT_TCP_MAX {
            eprintln!("[risc-box] nat: {NAT_TCP_MAX} outbound connections already open; dropping guest:{sport} -> {ip}:{dport}");
            return;
        }
        let addr = SocketAddr::from((ip, dport));
        match TcpStream::connect_timeout(&addr, NAT_CONNECT_TIMEOUT) {
            Ok(stream) => {
                if stream.set_nonblocking(true).is_err() {
                    return;
                }
                let _ = stream.set_nodelay(true);
                let rxb = tcp::SocketBuffer::new(vec![0; SPLICE_BUF]);
                let txb = tcp::SocketBuffer::new(vec![0; SPLICE_BUF]);
                let mut socket = tcp::Socket::new(rxb, txb);
                let local = IpListenEndpoint {
                    addr: Some(IpAddress::Ipv4(Ipv4Address::new(dst[0], dst[1], dst[2], dst[3]))),
                    port: dport,
                };
                if socket.listen(local).is_err() {
                    return;
                }
                let handle = self.sockets.add(socket);
                eprintln!("[risc-box] nat: guest:{sport} -> {ip}:{dport} connected");
                self.splices.push(Splice {
                    stream,
                    handle,
                    desc: format!("nat {ip}:{dport}"),
                    nat: Some((sport, dst, dport)),
                    created: Instant::now(),
                    outbuf: VecDeque::new(),
                    stream_eof: false,
                    eof_since: None,
                });
            }
            Err(e) => eprintln!("[risc-box] nat: guest:{sport} -> {ip}:{dport} refused: {e}"),
        }
    }

    /// Sends one guest datagram out its flow's real socket, creating the flow
    /// (and expiring idle ones) as needed.
    fn udp_nat_out(&mut self, mac: [u8; 6], sport: u16, dst: [u8; 4], dport: u16, payload: &[u8]) {
        if dport == 0 {
            return;
        }
        let now = Instant::now();
        if let Some(e) = self
            .udp_nat
            .iter_mut()
            .find(|e| e.guest_port == sport && e.dst_ip == dst && e.dst_port == dport)
        {
            e.last_used = now;
            let _ = e.sock.send(payload);
            return;
        }
        self.udp_nat.retain(|e| e.last_used.elapsed() < NAT_UDP_IDLE);
        if self.udp_nat.len() >= NAT_UDP_MAX {
            return; // full table: drop, like a router out of conntrack slots
        }
        let ip = Ipv4Addr::new(dst[0], dst[1], dst[2], dst[3]);
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
        if sock.connect((ip, dport)).is_err() || sock.set_nonblocking(true).is_err() {
            return;
        }
        let _ = sock.send(payload);
        self.udp_nat.push(UdpNat {
            sock,
            guest_port: sport,
            dst_ip: dst,
            dst_port: dport,
            guest_mac: mac,
            last_used: now,
        });
    }

    /// Drains every NAT flow's real socket, re-framing replies to the guest,
    /// and expires idle flows. Returns true if any datagram moved.
    fn poll_udp_nat(&mut self) -> bool {
        if self.udp_nat.is_empty() {
            return false;
        }
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; 2048];
        self.udp_nat.retain_mut(|e| {
            loop {
                match e.sock.recv(&mut buf) {
                    Ok(n) if n <= UDP_MAX_PAYLOAD => {
                        e.last_used = Instant::now();
                        frames.push(build_udp_frame(
                            e.guest_mac, e.dst_ip, e.dst_port, e.guest_port, &buf[..n],
                        ));
                    }
                    Ok(_) => {} // larger than the wire fits; drop (no fragmentation)
                    Err(ref err) if err.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break, // e.g. a surfaced ICMP refusal; idle expiry reaps
                }
            }
            e.last_used.elapsed() < NAT_UDP_IDLE
        });
        let moved = !frames.is_empty();
        for f in frames {
            self.push_to_guest(f);
        }
        moved
    }

    fn accept_new(&mut self) {
        let mut new_splices = Vec::new();
        for l in &self.listeners {
            loop {
                match l.listener.accept() {
                    Ok((stream, peer)) => {
                        if stream.set_nonblocking(true).is_err() {
                            continue;
                        }
                        let _ = stream.set_nodelay(true);
                        let rxb = tcp::SocketBuffer::new(vec![0; SPLICE_BUF]);
                        let txb = tcp::SocketBuffer::new(vec![0; SPLICE_BUF]);
                        let socket = tcp::Socket::new(rxb, txb);
                        let handle = self.sockets.add(socket);
                        self.ephemeral = if self.ephemeral >= 65500 { 49152 } else { self.ephemeral + 1 };
                        let local = self.ephemeral;
                        let sock = self.sockets.get_mut::<tcp::Socket>(handle);
                        let dest = IpEndpoint::new(IpAddress::Ipv4(guest_ip()), l.cfg.to);
                        match sock.connect(self.iface.context(), dest, local) {
                            Ok(()) => {
                                eprintln!(
                                    "[risc-box] forward tcp:{}: {peer} -> guest:{}",
                                    l.cfg.listen, l.cfg.to
                                );
                                new_splices.push(Splice {
                                    stream,
                                    handle,
                                    desc: format!("tcp:{} -> guest:{}", l.cfg.listen, l.cfg.to),
                                    nat: None,
                                    created: Instant::now(),
                                    outbuf: VecDeque::new(),
                                    stream_eof: false,
                                    eof_since: None,
                                });
                            }
                            Err(e) => {
                                eprintln!("[risc-box] forward connect failed: {e}");
                                self.sockets.remove(handle);
                            }
                        }
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        self.splices.extend(new_splices);
    }

    /// Moves bytes between real sockets and their smoltcp peers; reaps
    /// finished splices. Returns true if any bytes moved.
    fn shuttle(&mut self) -> bool {
        let mut moved = false;
        let sockets = &mut self.sockets;
        self.splices.retain_mut(|sp| {
            let sock = sockets.get_mut::<tcp::Socket>(sp.handle);

            // A NAT listener still in LISTEN is waiting for the guest to
            // finish the handshake — alive, but if the guest never comes
            // (SYN lost, process gone), reap it with its real connection.
            if sock.is_listening() {
                if sp.created.elapsed() > NAT_HANDSHAKE_GRACE {
                    sock.abort(); // falls through to teardown below
                } else {
                    return true;
                }
            }

            // real → guest
            if !sp.stream_eof && sock.can_send() {
                let room = sock.send_capacity() - sock.send_queue();
                if room > 0 {
                    let mut buf = vec![0u8; room.min(4096)];
                    match sp.stream.read(&mut buf) {
                        Ok(0) => {
                            sp.stream_eof = true;
                            sock.close(); // FIN toward the guest once we've sent all
                        }
                        Ok(n) => {
                            let sent = sock.send_slice(&buf[..n]).unwrap_or(0);
                            debug_assert!(sent == n, "send_slice under capacity cannot truncate");
                            moved = true;
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => {
                            sock.abort();
                        }
                    }
                }
            }

            // guest → real (stage into outbuf, then drain nonblocking)
            while sock.can_recv() && sp.outbuf.len() < SPLICE_BUF {
                let taken = sock
                    .recv(|bytes| {
                        let n = bytes.len().min(SPLICE_BUF);
                        (n, bytes[..n].to_vec())
                    })
                    .unwrap_or_default();
                if taken.is_empty() {
                    break;
                }
                sp.outbuf.extend(taken);
            }
            while !sp.outbuf.is_empty() {
                let (a, _) = sp.outbuf.as_slices();
                match sp.stream.write(a) {
                    Ok(0) => break,
                    Ok(n) => {
                        sp.outbuf.drain(..n);
                        moved = true;
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        sock.abort();
                        sp.outbuf.clear();
                        break;
                    }
                }
            }

            // real client gone with nothing left to deliver: start the grace
            // timer, and abort the guest side when it expires
            if sp.stream_eof && sp.outbuf.is_empty() {
                let since = *sp.eof_since.get_or_insert_with(Instant::now);
                if since.elapsed() > HALF_CLOSE_GRACE {
                    sock.abort();
                }
            } else {
                sp.eof_since = None;
            }

            // teardown: guest side fully closed and everything flushed out
            let guest_done = !sock.is_active() && !sock.is_listening();
            if guest_done && sp.outbuf.is_empty() {
                let _ = sp.stream.shutdown(std::net::Shutdown::Both);
                sockets.remove(sp.handle);
                eprintln!("[risc-box] {} closed", sp.desc);
                return false;
            }
            true
        });
        moved
    }

    // ---- DHCP server: one static lease, enough for one guest ---------------

    fn serve_dhcp(&mut self) {
        let sock = self.sockets.get_mut::<udp::Socket>(self.dhcp);
        let mut replies: Vec<Vec<u8>> = Vec::new();
        while let Ok((req, _meta)) = sock.recv() {
            if let Some(resp) = build_dhcp_reply(req) {
                replies.push(resp);
            }
        }
        for r in replies {
            let dst = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::BROADCAST), 68);
            let _ = sock.send_slice(&r, dst);
        }
    }

    // ---- DNS proxy at 10.0.2.2:53 -------------------------------------------
    // The guest's lease points here (DHCP option 6). Queries are answered
    // with the platform's own name lookup (`ToSocketAddrs` → wasi
    // ip-name-lookup), which works even where raw UDP egress does not — and
    // keeps resolution where the platform's SSRF checks live. A records only;
    // AAAA gets an empty NOERROR so dual-stack guests fall back to IPv4 (the
    // only family the guest wire carries).

    fn serve_dns(&mut self) {
        let Some(handle) = self.dns else { return };
        let sock = self.sockets.get_mut::<udp::Socket>(handle);
        let mut replies: Vec<(Vec<u8>, IpEndpoint)> = Vec::new();
        while let Ok((req, meta)) = sock.recv() {
            // The lookup inside blocks the loop briefly; only A queries
            // for real names pay it, and the platform resolver is near.
            if let Some(resp) = answer_dns(req) {
                replies.push((resp, meta.endpoint));
            }
        }
        for (r, dst) in replies {
            let _ = sock.send_slice(&r, dst);
        }
    }
}

/// Parses a BOOTP/DHCP request and builds the reply (offer for discover, ack
/// for request), leasing GUEST_IP with HOST_IP as router/server. Returns None
/// for anything malformed or not addressed to a DHCP server.
fn build_dhcp_reply(req: &[u8]) -> Option<Vec<u8>> {
    if req.len() < 240 || req[0] != 1 || &req[236..240] != &[0x63, 0x82, 0x53, 0x63] {
        return None;
    }
    // find option 53 (dhcp message type)
    let mut msg_type = None;
    let mut i = 240;
    while i + 1 < req.len() {
        let (opt, len) = (req[i], req[i + 1] as usize);
        if opt == 255 {
            break;
        }
        if opt == 0 {
            i += 1;
            continue;
        }
        if opt == 53 && len == 1 && i + 2 < req.len() {
            msg_type = Some(req[i + 2]);
        }
        i += 2 + len;
    }
    let reply_type: u8 = match msg_type? {
        1 => 2, // discover -> offer
        3 => 5, // request -> ack
        _ => return None,
    };

    let mut r = vec![0u8; 240];
    r[0] = 2; // BOOTREPLY
    r[1] = 1; // ethernet
    r[2] = 6; // hlen
    r[4..8].copy_from_slice(&req[4..8]); // xid
    r[10..12].copy_from_slice(&req[10..12]); // flags (keep broadcast bit)
    r[16..20].copy_from_slice(&GUEST_IP); // yiaddr
    r[20..24].copy_from_slice(&HOST_IP); // siaddr
    r[28..44].copy_from_slice(&req[28..44]); // chaddr (+padding)
    r[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    let mut opt = |code: u8, data: &[u8]| {
        r.push(code);
        r.push(data.len() as u8);
        r.extend_from_slice(data);
    };
    opt(53, &[reply_type]);
    opt(54, &HOST_IP); // server id
    opt(51, &DHCP_LEASE_SECS.to_be_bytes());
    opt(1, &[255, 255, 255, 0]);
    opt(3, &HOST_IP); // router: the gateway NATs outbound flows (when enabled)
    opt(6, &HOST_IP); // dns: the in-gateway proxy (answers only when outbound is on)
    r.push(255);
    Some(r)
}

// ---- frame and DNS builders (pure; unit-tested at the bottom) ---------------

/// RFC 1071 ones'-complement checksum over `data` (IP header, ICMP message).
fn inet_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [b] = chunks.remainder() {
        sum += (*b as u32) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Builds the echo reply for a guest echo request frame: MACs and IPs
/// swapped, TTL reset, type 8 → 0, both checksums recomputed. `end` bounds
/// the ICMP message (the frame may carry ethernet padding past it).
fn icmp_echo_reply(f: &[u8], ihl: usize, end: usize) -> Vec<u8> {
    let l4 = 14 + ihl;
    let mut r = f[..end].to_vec();
    r[0..6].copy_from_slice(&f[6..12]); // eth dst: the guest
    r[6..12].copy_from_slice(&HOST_MAC);
    r[26..30].copy_from_slice(&f[30..34]); // ip src: the pinged host
    r[30..34].copy_from_slice(&f[26..30]); // ip dst: the guest
    r[22] = 64; // ttl
    r[24] = 0;
    r[25] = 0;
    let ipck = inet_checksum(&r[14..14 + ihl]);
    r[24..26].copy_from_slice(&ipck.to_be_bytes());
    r[l4] = 0; // echo reply
    r[l4 + 2] = 0;
    r[l4 + 3] = 0;
    let ick = inet_checksum(&r[l4..]);
    r[l4 + 2..l4 + 4].copy_from_slice(&ick.to_be_bytes());
    r
}

/// Frames one NAT reply datagram for the guest: ethernet + IPv4 + UDP with
/// the external endpoint as the source. UDP checksum 0 (= none, valid for
/// IPv4) spares the pseudo-header arithmetic.
fn build_udp_frame(
    guest_mac: [u8; 6],
    src_ip: [u8; 4],
    src_port: u16,
    guest_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let ip_len = 20 + 8 + payload.len();
    let mut f = Vec::with_capacity(14 + ip_len);
    f.extend_from_slice(&guest_mac);
    f.extend_from_slice(&HOST_MAC);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend_from_slice(&[0x45, 0]); // v4, ihl 5, tos 0
    f.extend_from_slice(&(ip_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0, 0x40, 0]); // id 0, DF
    f.extend_from_slice(&[64, 17, 0, 0]); // ttl, udp, cksum placeholder
    f.extend_from_slice(&src_ip);
    f.extend_from_slice(&GUEST_IP);
    let ck = inet_checksum(&f[14..34]);
    f[24..26].copy_from_slice(&ck.to_be_bytes());
    f.extend_from_slice(&src_port.to_be_bytes());
    f.extend_from_slice(&guest_port.to_be_bytes());
    f.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]); // no UDP checksum (IPv4 allows it)
    f.extend_from_slice(payload);
    f
}

/// Answers one DNS query using the platform's name lookup. A queries resolve
/// (rcode 3 when the lookup fails); AAAA and everything else get an empty
/// NOERROR so resolvers move on to the A answer. None = not worth answering
/// (malformed, or itself a response).
fn answer_dns(req: &[u8]) -> Option<Vec<u8>> {
    if req.len() < 12 || req[2] & 0x80 != 0 {
        return None;
    }
    if u16::from_be_bytes([req[4], req[5]]) != 1 {
        return None; // exactly one question, like every real resolver sends
    }
    let mut i = 12;
    let mut name = String::new();
    loop {
        let len = *req.get(i)? as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len > 63 || name.len() > 253 {
            return None; // compression pointer or overlong: not a plain question
        }
        let label = req.get(i + 1..i + 1 + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        name.push_str(std::str::from_utf8(label).ok()?);
        i += 1 + len;
    }
    let qtype = u16::from_be_bytes([*req.get(i)?, *req.get(i + 1)?]);
    req.get(i + 3)?; // qclass present
    let question = &req[12..i + 4];

    let (rcode, answers): (u8, Vec<[u8; 4]>) = match qtype {
        1 => match (name.as_str(), 0u16).to_socket_addrs() {
            Ok(addrs) => (
                0,
                addrs
                    .filter_map(|a| match a.ip() {
                        IpAddr::V4(ip) => Some(ip.octets()),
                        IpAddr::V6(_) => None,
                    })
                    .take(DNS_MAX_ANSWERS)
                    .collect(),
            ),
            Err(_) => (3, Vec::new()), // NXDOMAIN: fail fast, no second resolver to try
        },
        _ => (0, Vec::new()),
    };

    let mut resp = Vec::with_capacity(12 + question.len() + 16 * answers.len());
    resp.extend_from_slice(&req[0..2]); // id
    resp.extend_from_slice(&(0x8180u16 | rcode as u16).to_be_bytes()); // QR|RD|RA
    resp.extend_from_slice(&1u16.to_be_bytes());
    resp.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    resp.extend_from_slice(&[0, 0, 0, 0]); // ns, ar
    resp.extend_from_slice(question);
    for a in &answers {
        resp.extend_from_slice(&[0xc0, 0x0c]); // name: pointer to the question
        resp.extend_from_slice(&1u16.to_be_bytes()); // A
        resp.extend_from_slice(&1u16.to_be_bytes()); // IN
        resp.extend_from_slice(&DNS_TTL_SECS.to_be_bytes());
        resp.extend_from_slice(&4u16.to_be_bytes());
        resp.extend_from_slice(a);
    }
    Some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed guest echo request to 8.8.8.8, checksums valid.
    fn echo_request() -> Vec<u8> {
        let payload = b"abcdefghijklmnop";
        let mut f = Vec::new();
        f.extend_from_slice(&HOST_MAC); // eth dst: the gateway
        f.extend_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // eth src: guest
        f.extend_from_slice(&[0x08, 0x00]);
        f.extend_from_slice(&[0x45, 0]);
        f.extend_from_slice(&((20 + 8 + payload.len()) as u16).to_be_bytes());
        f.extend_from_slice(&[0, 7, 0x40, 0]);
        f.extend_from_slice(&[64, 1, 0, 0]);
        f.extend_from_slice(&GUEST_IP);
        f.extend_from_slice(&[8, 8, 8, 8]);
        let ipck = inet_checksum(&f[14..34]);
        f[24..26].copy_from_slice(&ipck.to_be_bytes());
        f.extend_from_slice(&[8, 0, 0, 0, 0x12, 0x34, 0, 1]); // type 8, id, seq
        f.extend_from_slice(payload);
        let ick = inet_checksum(&f[34..]);
        f[36..38].copy_from_slice(&ick.to_be_bytes());
        f
    }

    #[test]
    fn checksum_validates_to_zero() {
        let f = echo_request();
        // a correct ones'-complement checksum makes the region sum to zero
        assert_eq!(inet_checksum(&f[14..34]), 0);
        assert_eq!(inet_checksum(&f[34..]), 0);
    }

    #[test]
    fn echo_reply_swaps_and_checksums() {
        let req = echo_request();
        let r = icmp_echo_reply(&req, 20, req.len());
        assert_eq!(&r[0..6], &req[6..12], "eth dst is the guest");
        assert_eq!(&r[6..12], &HOST_MAC);
        assert_eq!(&r[26..30], &[8, 8, 8, 8], "ip src is the pinged host");
        assert_eq!(&r[30..34], &GUEST_IP);
        assert_eq!(r[34], 0, "echo reply type");
        assert_eq!(&r[38..42], &req[38..42], "id/seq preserved");
        assert_eq!(&r[42..], &req[42..], "payload preserved");
        assert_eq!(inet_checksum(&r[14..34]), 0);
        assert_eq!(inet_checksum(&r[34..]), 0);
    }

    #[test]
    fn udp_frame_shape() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let f = build_udp_frame(mac, [1, 1, 1, 1], 53, 40000, b"hello");
        assert_eq!(f.len(), 14 + 20 + 8 + 5);
        assert_eq!(&f[0..6], &mac);
        assert_eq!(&f[12..14], &[0x08, 0x00]);
        assert_eq!(f[23], 17, "udp");
        assert_eq!(&f[26..30], &[1, 1, 1, 1]);
        assert_eq!(&f[30..34], &GUEST_IP);
        assert_eq!(inet_checksum(&f[14..34]), 0, "ip checksum valid");
        assert_eq!(u16::from_be_bytes([f[34], f[35]]), 53);
        assert_eq!(u16::from_be_bytes([f[36], f[37]]), 40000);
        assert_eq!(u16::from_be_bytes([f[38], f[39]]), 8 + 5);
        assert_eq!(&f[42..], b"hello");
    }

    /// name labels + qtype/qclass appended to a 12-byte query header.
    fn dns_query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0xbe, 0xef, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q
    }

    #[test]
    fn dns_a_query_ip_literal() {
        // an IP-literal "name" resolves without touching any resolver
        let resp = answer_dns(&dns_query("8.8.8.8", 1)).expect("answered");
        assert_eq!(&resp[0..2], &[0xbe, 0xef], "id echoed");
        assert_eq!(resp[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1, "one answer");
        assert_eq!(&resp[resp.len() - 4..], &[8, 8, 8, 8]);
    }

    #[test]
    fn dns_aaaa_gets_empty_noerror() {
        let resp = answer_dns(&dns_query("example.com", 28)).expect("answered");
        assert_eq!(resp[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0, "no answers");
    }

    #[test]
    fn dns_garbage_ignored() {
        assert!(answer_dns(&[0u8; 4]).is_none(), "truncated");
        let mut resp_flagged = dns_query("x.example", 1);
        resp_flagged[2] |= 0x80; // already a response
        assert!(answer_dns(&resp_flagged).is_none());
    }
}
