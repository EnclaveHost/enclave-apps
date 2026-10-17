//! Outbound TLS (rustls, pure-Rust RustCrypto provider, webpki roots) over
//! the async TCP stream, and a small keep-alive HTTP/1.1 client on top: the
//! ENS JSON-RPC calls, CCIP-Read gateways and DNS-over-HTTPS upstreams.

use crate::net::{self, Dest, Tcp};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;

thread_local! {
    static TLS_CFG: Arc<rustls::ClientConfig> = {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
                .with_safe_default_protocol_versions()
                .expect("tls versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    };
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
        let name = rustls::pki_types::ServerName::try_from(server_name.to_string())
            .map_err(|_| io::Error::other(format!("bad TLS server name {server_name}")))?;
        let cfg = TLS_CFG.with(|c| c.clone());
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
        let mut rd = &d[..];
        while !rd.is_empty() {
            self.conn.read_tls(&mut rd)?;
            self.conn.process_new_packets().map_err(tls_err)?;
        }
        Ok(true)
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

/// Idle keep-alive connections by origin.
#[derive(Default)]
pub struct Pool {
    idle: RefCell<HashMap<String, Vec<(Conn, u64)>>>,
}

const IDLE_MS: u64 = 25_000;
const MAX_BODY: usize = 4 << 20;

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
        let mut req = head.into_bytes();
        req.extend_from_slice(body);

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
            let mut c = match Conn::open(d, tls, limit).await {
                Ok(c) => c,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
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
