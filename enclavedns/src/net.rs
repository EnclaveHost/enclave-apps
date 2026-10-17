//! Async UDP and TCP over wasi:sockets, driven by wstd's wasi:io/poll
//! reactor. wstd has no UDP, and its TcpStream cannot dial IPv6 (the
//! platform's egress is IPv6-only), so both are thin wrappers here.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use wasip2::io::streams::{InputStream, OutputStream, StreamError};
use wasip2::sockets::instance_network::instance_network;
use wasip2::sockets::network::{ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress};
use wasip2::sockets::tcp::{ShutdownType, TcpSocket};
use wasip2::sockets::tcp_create_socket::create_tcp_socket;
use wasip2::sockets::udp::{IncomingDatagramStream, OutgoingDatagram, OutgoingDatagramStream, UdpSocket};
use wasip2::sockets::udp_create_socket::create_udp_socket;
use wstd::future::FutureExt;
use wstd::runtime::AsyncPollable;

pub fn err(e: ErrorCode) -> io::Error {
    use io::ErrorKind as K;
    let k = match e {
        ErrorCode::AccessDenied => K::PermissionDenied,
        ErrorCode::NotSupported => K::Unsupported,
        ErrorCode::InvalidArgument => K::InvalidInput,
        ErrorCode::OutOfMemory => K::OutOfMemory,
        ErrorCode::Timeout => K::TimedOut,
        ErrorCode::WouldBlock => K::WouldBlock,
        ErrorCode::AddressInUse => K::AddrInUse,
        ErrorCode::AddressNotBindable => K::AddrNotAvailable,
        ErrorCode::ConnectionRefused => K::ConnectionRefused,
        ErrorCode::ConnectionReset => K::ConnectionReset,
        ErrorCode::ConnectionAborted => K::ConnectionAborted,
        ErrorCode::RemoteUnreachable => K::HostUnreachable,
        _ => K::Other,
    };
    io::Error::new(k, format!("{e:?}"))
}

pub fn to_wasi(a: SocketAddr) -> IpSocketAddress {
    match a {
        SocketAddr::V4(v) => {
            let o = v.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress { port: v.port(), address: (o[0], o[1], o[2], o[3]) })
        }
        SocketAddr::V6(v) => {
            let s = v.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: v.port(),
                flow_info: 0,
                address: (s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]),
                scope_id: 0,
            })
        }
    }
}

pub fn from_wasi(a: IpSocketAddress) -> SocketAddr {
    match a {
        IpSocketAddress::Ipv4(v) => {
            let (a, b, c, d) = v.address;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), v.port)
        }
        IpSocketAddress::Ipv6(v) => {
            let (a, b, c, d, e, f, g, h) = v.address;
            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h)), v.port)
        }
    }
}

fn family(a: &SocketAddr) -> IpAddressFamily {
    if a.is_ipv4() {
        IpAddressFamily::Ipv4
    } else {
        IpAddressFamily::Ipv6
    }
}

async fn wait(p: wasip2::io::poll::Pollable) {
    AsyncPollable::new(p).wait_for().await
}

pub async fn sleep(d: Duration) {
    wstd::task::sleep(wstd::time::Duration::from_millis(d.as_millis() as u64)).await;
}

/// Run `f` for at most `d`.
pub async fn timeout<T>(d: Duration, f: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    match f.timeout(wstd::time::Duration::from_millis(d.as_millis() as u64)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
    }
}

// --------------------------------------------------------------------- UDP --

pub struct Udp {
    // child resources first: they must drop before the socket
    rx: IncomingDatagramStream,
    tx: OutgoingDatagramStream,
    _sock: UdpSocket,
}

impl Udp {
    pub async fn bind(addr: SocketAddr) -> io::Result<Udp> {
        let sock = create_udp_socket(family(&addr)).map_err(err)?;
        sock.start_bind(&instance_network(), to_wasi(addr)).map_err(err)?;
        loop {
            match sock.finish_bind() {
                Ok(()) => break,
                Err(ErrorCode::WouldBlock) => wait(sock.subscribe()).await,
                Err(e) => return Err(err(e)),
            }
        }
        let (rx, tx) = sock.stream(None).map_err(err)?;
        Ok(Udp { rx, tx, _sock: sock })
    }

    pub async fn recv(&self) -> io::Result<(Vec<u8>, SocketAddr)> {
        loop {
            let got = self.rx.receive(1).map_err(err)?;
            if let Some(d) = got.into_iter().next() {
                return Ok((d.data, from_wasi(d.remote_address)));
            }
            wait(self.rx.subscribe()).await;
        }
    }

    pub async fn send_to(&self, data: &[u8], to: SocketAddr) -> io::Result<()> {
        loop {
            if self.tx.check_send().map_err(err)? > 0 {
                let d = OutgoingDatagram { data: data.to_vec(), remote_address: Some(to_wasi(to)) };
                let n = self.tx.send(&[d]).map_err(err)?;
                if n == 1 {
                    return Ok(());
                }
            }
            wait(self.tx.subscribe()).await;
        }
    }
}

// --------------------------------------------------------------------- TCP --

pub struct Tcp {
    input: InputStream,
    output: OutputStream,
    sock: TcpSocket,
}

fn stream_err(e: StreamError) -> io::Error {
    match e {
        StreamError::Closed => io::Error::new(io::ErrorKind::BrokenPipe, "stream closed"),
        StreamError::LastOperationFailed(e) => io::Error::other(e.to_debug_string()),
    }
}

impl Tcp {
    pub async fn connect(addr: SocketAddr) -> io::Result<Tcp> {
        let sock = create_tcp_socket(family(&addr)).map_err(err)?;
        sock.start_connect(&instance_network(), to_wasi(addr)).map_err(err)?;
        loop {
            match sock.finish_connect() {
                Ok((input, output)) => {
                    return Ok(Tcp { input, output, sock });
                }
                Err(ErrorCode::WouldBlock) => wait(sock.subscribe()).await,
                Err(e) => return Err(err(e)),
            }
        }
    }

    /// Up to `max` bytes; an empty result is end of stream.
    pub async fn read(&self, max: usize) -> io::Result<Vec<u8>> {
        loop {
            match self.input.read(max as u64) {
                Ok(v) if !v.is_empty() => return Ok(v),
                Ok(_) => wait(self.input.subscribe()).await,
                Err(StreamError::Closed) => return Ok(Vec::new()),
                Err(e) => return Err(stream_err(e)),
            }
        }
    }

    pub async fn read_exact(&self, n: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let got = self.read(n - out.len()).await?;
            if got.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
            }
            out.extend_from_slice(&got);
        }
        Ok(out)
    }

    pub async fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let n = self.output.check_write().map_err(stream_err)? as usize;
            if n == 0 {
                wait(self.output.subscribe()).await;
                continue;
            }
            let k = n.min(data.len());
            self.output.write(&data[..k]).map_err(stream_err)?;
            data = &data[k..];
        }
        self.output.flush().map_err(stream_err)?;
        // check-write reports 0 until the flush has drained
        loop {
            match self.output.check_write() {
                Ok(0) => wait(self.output.subscribe()).await,
                Ok(_) => return Ok(()),
                Err(e) => return Err(stream_err(e)),
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.sock.shutdown(ShutdownType::Both);
    }
}

pub struct Listener {
    sock: TcpSocket,
}

impl Listener {
    pub async fn bind(addr: SocketAddr) -> io::Result<Listener> {
        let sock = create_tcp_socket(family(&addr)).map_err(err)?;
        sock.start_bind(&instance_network(), to_wasi(addr)).map_err(err)?;
        loop {
            match sock.finish_bind() {
                Ok(()) => break,
                Err(ErrorCode::WouldBlock) => wait(sock.subscribe()).await,
                Err(e) => return Err(err(e)),
            }
        }
        let _ = sock.set_listen_backlog_size(256);
        sock.start_listen().map_err(err)?;
        loop {
            match sock.finish_listen() {
                Ok(()) => break,
                Err(ErrorCode::WouldBlock) => wait(sock.subscribe()).await,
                Err(e) => return Err(err(e)),
            }
        }
        Ok(Listener { sock })
    }

    pub async fn accept(&self) -> io::Result<Tcp> {
        loop {
            match self.sock.accept() {
                Ok((sock, input, output)) => return Ok(Tcp { input, output, sock }),
                Err(ErrorCode::WouldBlock) => wait(self.sock.subscribe()).await,
                Err(e) => return Err(err(e)),
            }
        }
    }
}

// ------------------------------------------------------------ egress dial --

/// Where a dial goes: an address, or a name the egress front resolves.
#[derive(Clone, Debug)]
pub enum Dest {
    Addr(SocketAddr),
    Host(String, u16),
}

/// Dial out. With `ENCLAVE_EGRESS` (the platform's phase-1 SOCKS5 front,
/// `socks5h://<id>:<token>@host:port`) every connect goes through it;
/// otherwise the connect is direct (and, under the phase-2 toolchain,
/// transparently source-tagged by the host). A `Host` without the front is
/// resolved by `resolve_host` (our own resolver).
pub async fn dial(dest: &Dest, limit: Duration) -> io::Result<Tcp> {
    match std::env::var("ENCLAVE_EGRESS") {
        Ok(url) if !url.is_empty() => timeout(limit, socks_connect(&url, dest)).await,
        _ => match dest {
            Dest::Addr(a) => timeout(limit, Tcp::connect(*a)).await,
            Dest::Host(..) => Err(io::Error::other("a host name needs resolving before a direct dial")),
        },
    }
}

pub fn egress_proxy() -> bool {
    std::env::var("ENCLAVE_EGRESS").is_ok_and(|v| !v.is_empty())
}

async fn socks_connect(url: &str, dest: &Dest) -> io::Result<Tcp> {
    let bad = |m: &str| io::Error::other(format!("ENCLAVE_EGRESS: {m}"));
    let rest = url.split_once("://").map(|(_, r)| r).ok_or_else(|| bad("no scheme"))?;
    let (creds, front) = rest.rsplit_once('@').ok_or_else(|| bad("no credential"))?;
    let (id, token) = creds.split_once(':').ok_or_else(|| bad("bad credential"))?;
    let front: SocketAddr = front
        .trim_end_matches('/')
        .parse()
        .or_else(|_| {
            // "localhost:1080" style
            let (h, p) = front.rsplit_once(':').ok_or(())?;
            let p: u16 = p.trim_end_matches('/').parse().map_err(|_| ())?;
            if h == "localhost" {
                Ok(SocketAddr::from(([127, 0, 0, 1], p)))
            } else {
                Err(())
            }
        })
        .map_err(|_| bad("front must be ip:port"))?;
    let s = Tcp::connect(front).await?;
    s.write_all(&[5, 1, 2]).await?;
    if s.read_exact(2).await? != [5, 2] {
        return Err(bad("front refused user/pass auth"));
    }
    let mut auth = vec![1, id.len() as u8];
    auth.extend_from_slice(id.as_bytes());
    auth.push(token.len() as u8);
    auth.extend_from_slice(token.as_bytes());
    s.write_all(&auth).await?;
    if s.read_exact(2).await?[1] != 0 {
        return Err(bad("credential rejected"));
    }
    let mut req = vec![5, 1, 0];
    let port = match dest {
        Dest::Addr(SocketAddr::V4(a)) => {
            req.push(1);
            req.extend_from_slice(&a.ip().octets());
            a.port()
        }
        Dest::Addr(SocketAddr::V6(a)) => {
            req.push(4);
            req.extend_from_slice(&a.ip().octets());
            a.port()
        }
        Dest::Host(h, p) => {
            req.push(3);
            req.push(h.len() as u8);
            req.extend_from_slice(h.as_bytes());
            *p
        }
    };
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;
    let head = s.read_exact(4).await?;
    if head[1] != 0 {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("egress CONNECT refused (rep={})", head[1])));
    }
    let n = match head[3] {
        1 => 4,
        4 => 16,
        3 => s.read_exact(1).await?[0] as usize,
        _ => return Err(bad("bad BND.ADDR type")),
    };
    s.read_exact(n + 2).await?;
    Ok(s)
}

/// A random 16-bit value from wasi:random (query ids).
pub fn random_u16() -> u16 {
    let mut b = [0u8; 2];
    getrandom::getrandom(&mut b).expect("wasi:random");
    u16::from_be_bytes(b)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Monotonic milliseconds.
pub fn now_ms() -> u64 {
    wasip2::clocks::monotonic_clock::now() / 1_000_000
}

/// Drive several futures on this task until all finish (results in order).
pub async fn join_all<F: std::future::Future>(futs: Vec<F>) -> Vec<F::Output> {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    struct JoinAll<F: std::future::Future> {
        futs: Vec<Option<Pin<Box<F>>>>,
        out: Vec<Option<F::Output>>,
    }
    impl<F: std::future::Future> std::future::Future for JoinAll<F> {
        type Output = Vec<F::Output>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = unsafe { self.get_unchecked_mut() };
            let mut pending = false;
            for (i, slot) in this.futs.iter_mut().enumerate() {
                if let Some(f) = slot {
                    match f.as_mut().poll(cx) {
                        Poll::Ready(v) => {
                            this.out[i] = Some(v);
                            *slot = None;
                        }
                        Poll::Pending => pending = true,
                    }
                }
            }
            if pending {
                Poll::Pending
            } else {
                Poll::Ready(this.out.iter_mut().map(|o| o.take().unwrap()).collect())
            }
        }
    }
    let n = futs.len();
    JoinAll { futs: futs.into_iter().map(|f| Some(Box::pin(f))).collect(), out: (0..n).map(|_| None).collect() }.await
}

/// The host's own name lookup (wasi:sockets/ip-name-lookup). Inside an
/// isolated partition this is the only resolver that names the egress
/// forwarders: /etc/hosts maps each allowed origin to its loopback listener.
/// Elsewhere it is the host's ordinary resolver. Empty on any failure.
pub async fn system_lookup(name: &str) -> Vec<IpAddr> {
    use wasip2::sockets::ip_name_lookup::resolve_addresses;
    use wasip2::sockets::network::IpAddress;
    let Ok(stream) = resolve_addresses(&instance_network(), name) else { return Vec::new() };
    let mut out = Vec::new();
    let work = async {
        loop {
            match stream.resolve_next_address() {
                Ok(Some(IpAddress::Ipv4((a, b, c, d)))) => out.push(IpAddr::V4(Ipv4Addr::new(a, b, c, d))),
                Ok(Some(IpAddress::Ipv6((a, b, c, d, e, f, g, h)))) => out.push(IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h))),
                Ok(None) => return Ok(()),
                Err(ErrorCode::WouldBlock) => wait(stream.subscribe()).await,
                Err(e) => return Err(err(e)),
            }
        }
    };
    let _ = timeout(Duration::from_secs(5), work).await;
    out
}
