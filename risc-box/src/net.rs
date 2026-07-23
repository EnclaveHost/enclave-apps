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
//! There is no outbound NAT yet: the guest can talk to 10.0.2.2 and be
//! reached through the forwards, but cannot open connections to the wider
//! internet. That keeps the trust story simple (the machine cannot exfiltrate
//! anything by itself) and the code small.
//!
//!   browser/ssh ──tcp:2222──► app listener ──splice──► smoltcp ──frames──►
//!   virtio-net ──► guest kernel ──► sshd :22

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};

use riscv_emu_rust::net::NetBackend;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address,
};

pub const HOST_IP: [u8; 4] = [10, 0, 2, 2];
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
const HOST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xaa, 0xbb, 0x02];
const DHCP_LEASE_SECS: u32 = 7 * 24 * 3600;
const FRAME_QUEUE_CAP: usize = 256; // frames buffered each way before dropping
const SPLICE_BUF: usize = 64 * 1024;

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

struct Splice {
    stream: TcpStream,
    handle: SocketHandle,
    to_port: u16,
    outbuf: VecDeque<u8>, // guest → real socket, pending write
    stream_eof: bool,
    eof_since: Option<std::time::Instant>, // grace timer once the real side is done
}

// A guest process that never closes its end (e.g. a lingering CLOSE_WAIT)
// would pin the splice forever after the real client has gone; reap it once
// the client has been gone this long with nothing left to flush.
const HALF_CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

// ---- the stack -------------------------------------------------------------

pub struct NetStack {
    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    listeners: Vec<Listener>,
    splices: Vec<Splice>,
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
    pub fn new(forwards: &[ForwardCfg]) -> Self {
        let mut device = QueueDevice { rx: VecDeque::new(), tx: VecDeque::new() };
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(HOST_MAC)));
        let now = SmolInstant::from_millis(0);
        let mut iface = Interface::new(config, &mut device, now);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(host_ip()), 24))
                .expect("one address fits");
        });

        let mut sockets = SocketSet::new(vec![]);
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 4096]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 4096]);
        let mut dhcp_socket = udp::Socket::new(rx, tx);
        dhcp_socket.bind(67).expect("bind dhcp");
        let dhcp = sockets.add(dhcp_socket);

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
            listeners,
            splices: Vec::new(),
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

    /// One turn of the network: exchange frames with the emulator's backend,
    /// run smoltcp, serve DHCP, accept and shuttle forwards. Returns true if
    /// anything moved (the caller uses it to keep the guest un-throttled
    /// while traffic is in flight).
    pub fn pump(&mut self, backend: &mut dyn NetBackend) -> bool {
        let mut activity = false;

        while let Some(frame) = backend.host_pop() {
            self.rx_frames += 1;
            activity = true;
            if self.device.rx.len() < FRAME_QUEUE_CAP {
                self.device.rx.push_back(frame);
            }
        }

        self.accept_new();

        let now = SmolInstant::from_millis(self.epoch.elapsed().as_millis() as i64);
        self.iface.poll(now, &mut self.device, &mut self.sockets);

        self.serve_dhcp();
        if self.shuttle() {
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
                                    to_port: l.cfg.to,
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
                let since = *sp.eof_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > HALF_CLOSE_GRACE {
                    sock.abort();
                }
            } else {
                sp.eof_since = None;
            }

            // teardown: guest side fully closed and everything flushed out
            let guest_done = !sock.is_active();
            if guest_done && sp.outbuf.is_empty() {
                let _ = sp.stream.shutdown(std::net::Shutdown::Both);
                sockets.remove(sp.handle);
                eprintln!("[risc-box] forward to guest:{} closed", sp.to_port);
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
    opt(3, &HOST_IP); // router (only reaches the forwards, but harmless)
    opt(6, &HOST_IP); // dns (no resolver behind it yet)
    r.push(255);
    Some(r)
}
