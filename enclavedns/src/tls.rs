//! Outbound TLS (rustls, pure-Rust RustCrypto provider, webpki roots) over
//! the async TCP stream, and a small keep-alive HTTP/1.1 client on top: the
//! ENS JSON-RPC calls, CCIP-Read gateways and DNS-over-HTTPS upstreams.

use crate::net::{self, Dest, Tcp};
use bytes::Bytes;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

thread_local! {
    static TLS_CFG: Arc<rustls::ClientConfig> = tls_config(&[]);
    /// Offers h2 first: an origin that takes it gets one multiplexed connection.
    static TLS_CFG_H2: Arc<rustls::ClientConfig> = tls_config(&[b"h2", b"http/1.1"]);
}

fn tls_config(alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let mut c = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    c.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(c)
}

pub struct TlsConn {
    tcp: Tcp,
    conn: rustls::ClientConnection,
}

fn tls_err(e: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("tls: {e}"))
}

impl TlsConn {
    pub async fn handshake(tcp: Tcp, server_name: &str) -> io::Result<TlsConn> {
        Self::handshake_with(tcp, server_name, TLS_CFG.with(|c| c.clone())).await
    }

    async fn handshake_with(tcp: Tcp, server_name: &str, cfg: Arc<rustls::ClientConfig>) -> io::Result<TlsConn> {
        let name = rustls::pki_types::ServerName::try_from(server_name.to_string())
            .map_err(|_| io::Error::other(format!("bad TLS server name {server_name}")))?;
        let conn = rustls::ClientConnection::new(cfg, name).map_err(tls_err)?;
        let mut t = TlsConn { tcp, conn };
        while t.conn.is_handshaking() {
            t.flush_tls().await?;
            if t.conn.wants_read() {
                t.fill().await?;
            }
        }
        t.flush_tls().await?;
        Ok(t)
    }

    async fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            let mut out = Vec::new();
            self.conn.write_tls(&mut out)?;
            self.tcp.write_all(&out).await?;
        }
        Ok(())
    }

    /// Feed more ciphertext in; false at end of stream.
    async fn fill(&mut self) -> io::Result<bool> {
        let d = self.tcp.read(16 * 1024).await?;
        if d.is_empty() {
            return Ok(false);
        }
        self.feed(&d)?;
        Ok(true)
    }

    fn feed(&mut self, mut rd: &[u8]) -> io::Result<()> {
        while !rd.is_empty() {
            self.conn.read_tls(&mut rd)?;
            self.conn.process_new_packets().map_err(tls_err)?;
        }
        Ok(())
    }

    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.conn.writer().write_all(data)?;
        self.flush_tls().await
    }

    /// Plaintext; empty at end of stream.
    pub async fn read(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; max.max(1)];
        loop {
            match self.conn.reader().read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    return Ok(buf);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !self.fill().await? {
                        return Ok(Vec::new());
                    }
                    self.flush_tls().await?;
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Vec::new()),
                Err(e) => return Err(e),
            }
        }
    }
}

/// A byte stream that may or may not be TLS.
pub enum Conn {
    Plain(Tcp),
    Tls(Box<TlsConn>),
}

impl Conn {
    pub async fn write_all(&mut self, d: &[u8]) -> io::Result<()> {
        match self {
            Conn::Plain(t) => t.write_all(d).await,
            Conn::Tls(t) => t.write_all(d).await,
        }
    }

    pub async fn read(&mut self, max: usize) -> io::Result<Vec<u8>> {
        match self {
            Conn::Plain(t) => t.read(max).await,
            Conn::Tls(t) => t.read(max).await,
        }
    }

    pub async fn read_exact(&mut self, n: usize, buf: &mut Vec<u8>) -> io::Result<Vec<u8>> {
        while buf.len() < n {
            let got = self.read(64 * 1024).await?;
            if got.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
            }
            buf.extend_from_slice(&got);
        }
        let rest = buf.split_off(n);
        Ok(std::mem::replace(buf, rest))
    }

    pub async fn open(dest: &Dest, tls_name: Option<&str>, limit: Duration) -> io::Result<Conn> {
        let tcp = net::dial(dest, limit).await?;
        match tls_name {
            None => Ok(Conn::Plain(tcp)),
            Some(n) => Ok(Conn::Tls(Box::new(net::timeout(limit, TlsConn::handshake(tcp, n)).await?))),
        }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(u: &str) -> Result<Url, String> {
    let (https, rest) = if let Some(r) = u.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = u.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("not an http(s) URL: {u}"));
    };
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if auth.is_empty() || auth.contains('@') {
        return Err(format!("bad URL authority in {u}"));
    }
    let default = if https { 443 } else { 80 };
    let (host, port) = if let Some(r) = auth.strip_prefix('[') {
        let (h, t) = r.split_once(']').ok_or("bad IPv6 URL")?;
        (h.to_string(), t.strip_prefix(':').map(|p| p.parse().map_err(|_| "bad port")).transpose()?.unwrap_or(default))
    } else if let Some((h, p)) = auth.rsplit_once(':') {
        (h.to_string(), p.parse().map_err(|_| "bad port")?)
    } else {
        (auth.to_string(), default)
    };
    Ok(Url { https, host: host.to_ascii_lowercase(), port, path: path.to_string() })
}

/// Outbound HTTP by origin: one multiplexed HTTP/2 connection for each
/// origin that negotiates h2, else idle HTTP/1.1 keep-alive connections.
/// Under the platform's egress a new connection costs several slow round
/// trips, so concurrent requests must share one rather than each dial.
#[derive(Default)]
pub struct Pool {
    idle: RefCell<HashMap<String, Vec<(Conn, u64)>>>,
    h2: RefCell<HashMap<String, H2Conn>>,
    /// one dial per origin at a time; the others wait and share it
    h2_dialing: RefCell<HashMap<String, Rc<tokio::sync::Mutex<()>>>>,
    /// origins whose ALPN chose HTTP/1.1, until (ms)
    h1_only: RefCell<HashMap<String, u64>>,
    pub stats: RefCell<PoolStats>,
}

#[derive(Default, Clone, Debug)]
pub struct PoolStats {
    pub h2_connects: u64,
    pub h2_requests: u64,
    pub h2_retries: u64,
    pub h1_connects: u64,
    pub h1_requests: u64,
    /// TCP + TLS time of the last new connection
    pub connect_ms_last: u64,
    pub connect_ms_max: u64,
}

#[derive(Clone)]
struct H2Conn {
    send: h2::client::SendRequest<Bytes>,
    life: Rc<H2Life>,
}

struct H2Life {
    dead: Cell<bool>,
    used_ms: Cell<u64>,
}

const IDLE_MS: u64 = 25_000;
const MAX_BODY: usize = 4 << 20;
/// An HTTP/2 connection is pinged while in use and let go after this idle.
const H2_IDLE_MS: u64 = 300_000;
const H2_PING_EVERY: Duration = Duration::from_secs(15);

impl Pool {
    fn take(&self, key: &str) -> Option<Conn> {
        let now = net::now_ms();
        let mut m = self.idle.borrow_mut();
        let v = m.get_mut(key)?;
        while let Some((c, t)) = v.pop() {
            if now.saturating_sub(t) < IDLE_MS {
                return Some(c);
            }
        }
        None
    }

    fn put(&self, key: &str, c: Conn) {
        let mut m = self.idle.borrow_mut();
        let v = m.entry(key.to_string()).or_default();
        if v.len() < 8 {
            v.push((c, net::now_ms()));
        }
    }

    /// One HTTP/1.1 exchange. `dests` are where the host may be dialled (one
    /// `Host` dest under the egress front, or resolved addresses).
    pub async fn request(
        &self,
        url: &Url,
        dests: &[Dest],
        method: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        limit: Duration,
    ) -> io::Result<Response> {
        let key = format!("{}://{}:{}", if url.https { "https" } else { "http" }, url.host, url.port);
        let host_hdr = if (url.https && url.port == 443) || (!url.https && url.port == 80) {
            url.host.clone()
        } else {
            format!("{}:{}", url.host, url.port)
        };
        let mut head = format!("{method} {} HTTP/1.1\r\nhost: {host_hdr}\r\nuser-agent: EnclaveDNS/0.1\r\n", url.path);
        for (k, v) in headers {
            if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
                return Err(io::Error::other("bad header"));
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if method != "GET" || !body.is_empty() {
            head.push_str(&format!("content-length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        if url.https && !self.is_h1_only(&key) {
            if let Some(r) = self.h2_request(&key, url, &host_hdr, dests, method, headers, body, limit).await? {
                return Ok(r);
            }
        }
        let mut req = head.into_bytes();
        req.extend_from_slice(body);
        self.stats.borrow_mut().h1_requests += 1;

        // a pooled connection the peer closed fails before any byte: retry fresh once
        if let Some(mut c) = self.take(&key) {
            if let Ok(Ok((resp, keep))) = net::timeout(limit, async { Ok(exchange(&mut c, &req, method).await) }).await {
                if keep {
                    self.put(&key, c);
                }
                return Ok(resp);
            }
        }
        let mut last = io::Error::other("no address to dial");
        for d in dests {
            let tls = if url.https { Some(url.host.as_str()) } else { None };
            let t0 = net::now_ms();
            let mut c = match Conn::open(d, tls, limit).await {
                Ok(c) => c,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            self.note_connect(t0, false);
            let r = net::timeout(limit, async { exchange(&mut c, &req, method).await }).await;
            match r {
                Ok((resp, keep)) => {
                    if keep {
                        self.put(&key, c);
                    }
                    return Ok(resp);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

impl Pool {
    fn note_connect(&self, t0: u64, h2: bool) {
        let ms = net::now_ms().saturating_sub(t0);
        let mut s = self.stats.borrow_mut();
        if h2 {
            s.h2_connects += 1;
        } else {
            s.h1_connects += 1;
        }
        s.connect_ms_last = ms;
        s.connect_ms_max = s.connect_ms_max.max(ms);
    }

    fn is_h1_only(&self, key: &str) -> bool {
        self.h1_only.borrow().get(key).is_some_and(|until| *until > net::now_ms())
    }

    fn live_h2(&self, key: &str) -> Option<H2Conn> {
        let mut m = self.h2.borrow_mut();
        match m.get(key) {
            Some(c) if !c.life.dead.get() => Some(c.clone()),
            Some(_) => {
                m.remove(key);
                None
            }
            None => None,
        }
    }

    /// One request over the origin's HTTP/2 connection; None when the
    /// origin does not speak h2 (the caller uses HTTP/1.1).
    #[allow(clippy::too_many_arguments)]
    async fn h2_request(
        &self,
        key: &str,
        url: &Url,
        authority: &str,
        dests: &[Dest],
        method: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        limit: Duration,
    ) -> io::Result<Option<Response>> {
        let deadline = net::now_ms() + limit.as_millis() as u64;
        let left = || Duration::from_millis(deadline.saturating_sub(net::now_ms()));
        let mut retried = false;
        loop {
            let Some((c, fresh)) = self.h2_conn(key, url, dests, left()).await? else { return Ok(None) };
            self.stats.borrow_mut().h2_requests += 1;
            let r = net::timeout(left(), h2_exchange(c.send.clone(), url, authority, method, headers, body)).await;
            match r {
                Ok(resp) => {
                    c.life.used_ms.set(net::now_ms());
                    return Ok(Some(resp));
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => return Err(e),
                Err(e) => {
                    // the connection failed (GOAWAY, reset, closed by the
                    // path while idle): retire it and try once on a new one
                    c.life.dead.set(true);
                    if fresh || retried {
                        return Err(e);
                    }
                    retried = true;
                    self.stats.borrow_mut().h2_retries += 1;
                }
            }
        }
    }

    /// The origin's live HTTP/2 connection, dialling it if there is none:
    /// (connection, newly dialled), or None for an HTTP/1.1-only origin.
    async fn h2_conn(&self, key: &str, url: &Url, dests: &[Dest], limit: Duration) -> io::Result<Option<(H2Conn, bool)>> {
        if let Some(c) = self.live_h2(key) {
            return Ok(Some((c, false)));
        }
        let lock = self.h2_dialing.borrow_mut().entry(key.to_string()).or_default().clone();
        let _dialing = net::timeout(limit, async { Ok(lock.lock().await) }).await?;
        if let Some(c) = self.live_h2(key) {
            return Ok(Some((c, false)));
        }
        if self.is_h1_only(key) {
            return Ok(None);
        }
        let deadline = net::now_ms() + limit.as_millis() as u64;
        let left = || Duration::from_millis(deadline.saturating_sub(net::now_ms()));
        let mut last = io::Error::other("no address to dial");
        for d in dests {
            let t0 = net::now_ms();
            let tcp = match net::dial(d, left()).await {
                Ok(t) => t,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let cfg = TLS_CFG_H2.with(|c| c.clone());
            let tls = match net::timeout(left(), TlsConn::handshake_with(tcp, &url.host, cfg)).await {
                Ok(t) => t,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            if tls.conn.alpn_protocol() != Some(b"h2") {
                // not wasted: it is a good HTTP/1.1 connection
                self.note_connect(t0, false);
                self.h1_only.borrow_mut().insert(key.to_string(), net::now_ms() + 600_000);
                self.put(key, Conn::Tls(Box::new(tls)));
                return Ok(None);
            }
            let c = net::timeout(left(), h2_start(tls)).await?;
            self.note_connect(t0, true);
            self.h2.borrow_mut().insert(key.to_string(), c.clone());
            return Ok(Some((c, true)));
        }
        Err(last)
    }
}

fn h2_err(e: h2::Error) -> io::Error {
    if e.is_io() {
        e.into_io().unwrap_or_else(|| io::Error::other("h2 i/o"))
    } else {
        io::Error::other(format!("h2: {e}"))
    }
}

/// Run h2 over a TLS connection: h2 speaks to one end of an in-memory
/// duplex, a pump task moves its bytes through rustls and the socket.
async fn h2_start(tls: TlsConn) -> io::Result<H2Conn> {
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let life = Rc::new(H2Life { dead: Cell::new(false), used_ms: Cell::new(net::now_ms()) });
    wstd::runtime::spawn(pump(tls, theirs, life.clone())).detach();
    let (send, mut conn) = h2::client::Builder::new()
        .initial_window_size(1 << 20)
        .initial_connection_window_size(4 << 20)
        .handshake::<_, Bytes>(ours)
        .await
        .map_err(h2_err)?;
    let ping = conn.ping_pong();
    let l = life.clone();
    wstd::runtime::spawn(async move {
        let _ = conn.await;
        l.dead.set(true);
    })
    .detach();
    if let Some(p) = ping {
        wstd::runtime::spawn(keepalive(p, life.clone())).detach();
    }
    Ok(H2Conn { send, life })
}

/// Ping while in use: a path that silently dropped the connection shows
/// up here, not as a request that hangs until its deadline.
async fn keepalive(mut p: h2::PingPong, life: Rc<H2Life>) {
    loop {
        net::sleep(H2_PING_EVERY).await;
        if life.dead.get() {
            return;
        }
        if net::now_ms().saturating_sub(life.used_ms.get()) > H2_IDLE_MS {
            life.dead.set(true);
            return;
        }
        let pong = net::timeout(Duration::from_secs(5), async { p.ping(h2::Ping::opaque()).await.map_err(h2_err) }).await;
        if pong.is_err() {
            life.dead.set(true);
            return;
        }
    }
}

async fn pump(mut tls: TlsConn, mut side: tokio::io::DuplexStream, life: Rc<H2Life>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut plain = vec![0u8; 16 * 1024];
    let mut out = vec![0u8; 16 * 1024];
    'run: loop {
        loop {
            match tls.conn.reader().read(&mut plain) {
                Ok(0) => break 'run,
                Ok(n) => {
                    if side.write_all(&plain[..n]).await.is_err() {
                        break 'run;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break 'run,
            }
        }
        if tls.flush_tls().await.is_err() || life.dead.get() {
            break;
        }
        let tcp = &tls.tcp;
        match net::select2(side.read(&mut out), tcp.read(16 * 1024)).await {
            net::Either::Left(Ok(0)) | net::Either::Left(Err(_)) => {
                tls.conn.send_close_notify();
                let _ = tls.flush_tls().await;
                break;
            }
            net::Either::Left(Ok(n)) => {
                if tls.conn.writer().write_all(&out[..n]).is_err() {
                    break;
                }
            }
            net::Either::Right(Ok(d)) if !d.is_empty() => {
                if tls.feed(&d).is_err() {
                    break;
                }
            }
            net::Either::Right(_) => break,
        }
    }
    life.dead.set(true);
    tls.tcp.shutdown();
}

async fn h2_exchange(
    send: h2::client::SendRequest<Bytes>,
    url: &Url,
    authority: &str,
    method: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<Response> {
    let mut b = http::Request::builder()
        .method(method)
        .uri(format!("https://{authority}{}", url.path))
        .header("user-agent", "EnclaveDNS/0.1");
    for (k, v) in headers {
        // connection-specific fields are forbidden in HTTP/2
        if ["connection", "host", "keep-alive", "transfer-encoding", "upgrade", "proxy-connection"].contains(&k.to_ascii_lowercase().as_str()) {
            continue;
        }
        b = b.header(*k, *v);
    }
    if !body.is_empty() || method != "GET" {
        b = b.header("content-length", body.len());
    }
    let req = b.body(()).map_err(io::Error::other)?;
    let mut send = send.ready().await.map_err(h2_err)?;
    let (resp, mut stream) = send.send_request(req, body.is_empty()).map_err(h2_err)?;
    if !body.is_empty() {
        stream.send_data(Bytes::copy_from_slice(body), true).map_err(h2_err)?;
    }
    let resp = resp.await.map_err(h2_err)?;
    let status = resp.status().as_u16();
    let mut rb = resp.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = rb.data().await {
        let chunk = chunk.map_err(h2_err)?;
        let _ = rb.flow_control().release_capacity(chunk.len());
        if out.len() + chunk.len() > MAX_BODY {
            return Err(io::Error::other("response body too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Response { status, body: out })
}

async fn exchange(c: &mut Conn, req: &[u8], method: &str) -> io::Result<(Response, bool)> {
    c.write_all(req).await?;
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::other("response head too large"));
        }
        let got = c.read(16 * 1024).await?;
        if got.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed before a response"));
        }
        buf.extend_from_slice(&got);
    };
    let text = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
    let mut lines = text.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("bad status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let mut rest = buf.split_off(head_end);
    let hv = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.to_ascii_lowercase());
    let mut keep = hv("connection").map_or(true, |v| !v.contains("close"));
    let body = if method == "HEAD" || status == 204 || status == 304 {
        Vec::new()
    } else if hv("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        let mut out = Vec::new();
        loop {
            let line_end = loop {
                if let Some(p) = rest.windows(2).position(|w| w == b"\r\n") {
                    break p;
                }
                let got = c.read(16 * 1024).await?;
                if got.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "chunked body cut short"));
                }
                rest.extend_from_slice(&got);
            };
            let size_txt = String::from_utf8_lossy(&rest[..line_end]).to_string();
            let size = usize::from_str_radix(size_txt.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| io::Error::other("bad chunk size"))?;
            rest.drain(..line_end + 2);
            if size == 0 {
                // trailers until the blank line
                loop {
                    if let Some(p) = rest.windows(2).position(|w| w == b"\r\n") {
                        let blank = p == 0;
                        rest.drain(..p + 2);
                        if blank {
                            break;
                        }
                        continue;
                    }
                    let got = c.read(4096).await?;
                    if got.is_empty() {
                        break;
                    }
                    rest.extend_from_slice(&got);
                }
                break;
            }
            if out.len() + size > MAX_BODY {
                return Err(io::Error::other("response body too large"));
            }
            let chunk = c.read_exact(size + 2, &mut rest).await?;
            out.extend_from_slice(&chunk[..size]);
        }
        out
    } else if let Some(n) = hv("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if n > MAX_BODY {
            return Err(io::Error::other("response body too large"));
        }
        c.read_exact(n, &mut rest).await?
    } else {
        keep = false;
        loop {
            let got = c.read(64 * 1024).await?;
            if got.is_empty() {
                break;
            }
            rest.extend_from_slice(&got);
            if rest.len() > MAX_BODY {
                return Err(io::Error::other("response body too large"));
            }
        }
        rest
    };
    Ok((Response { status, body }, keep))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = parse_url("https://Eth.LlamaRPC.com").unwrap();
        assert!(u.https && u.host == "eth.llamarpc.com" && u.port == 443 && u.path == "/");
        let u = parse_url("http://[2001:db8::1]:8545/rpc?x=1").unwrap();
        assert!(!u.https && u.host == "2001:db8::1" && u.port == 8545 && u.path == "/rpc?x=1");
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("https://user@x").is_err());
    }
}
