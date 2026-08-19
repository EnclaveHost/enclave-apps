//! Minimal outbound HTTP/1.1 client over the fleet egress: the s3.rs
//! request engine generalized to arbitrary URLs (delegated-routing PUTs,
//! value-source fetches). https uses rustls with the RustCrypto provider,
//! the one TLS stack that builds for wasm32-wasip2; http stays a plain
//! TcpStream (local kubo, e2e rigs).
//!
//! Blocking by design, bounded by a wall-clock deadline: the event loop
//! runs at most one request per tick (the s3-ipfs-adapter doctrine), and
//! connect_timeout is not trusted on wasip2 — the read timeout plus the
//! deadline bound every request instead.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub path: String, // path + query, always starts with '/'
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("url must be http(s)://…, got {url}"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        if authority.is_empty() {
            return Err("url has no host".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (h.to_string(), p.parse().map_err(|_| "bad port")?)
            }
            _ => (authority.to_string(), if https { 443 } else { 80 }),
        };
        Ok(Url { https, host, port, path })
    }

    /// The same origin with a different path.
    pub fn with_path(&self, path: String) -> Url {
        Url { path, ..self.clone() }
    }

    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
}

enum Wire {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}

fn connect(url: &Url) -> Result<Wire, String> {
    let sock = crate::egress::dial(&url.host, url.port, None)?;
    let _ = sock.set_read_timeout(Some(Duration::from_secs(20)));
    if !url.https {
        return Ok(Wire::Plain(sock));
    }
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls versions: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(url.host.clone())
        .map_err(|_| format!("bad TLS server name {}", url.host))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("tls setup: {e}"))?;
    Ok(Wire::Tls(Box::new(rustls::StreamOwned::new(conn, sock))))
}

/// One request, one connection. Returns (status, body, lowercased headers).
pub fn request(
    method: &str,
    url: &Url,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(u16, Vec<u8>, Vec<(String, String)>), String> {
    let host_header = {
        let default = if url.https { 443 } else { 80 };
        if url.port == default {
            url.host.clone()
        } else {
            format!("{}:{}", url.host, url.port)
        }
    };
    let mut head = format!("{method} {} HTTP/1.1\r\nhost: {host_header}\r\n", url.path);
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", body.len()));

    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut wire = connect(url)?;
    wire.write_all(head.as_bytes()).map_err(|e| format!("send: {e}"))?;
    for chunk in body.chunks(64 * 1024) {
        wire.write_all(chunk).map_err(|e| format!("send body: {e}"))?;
    }
    wire.flush().ok();

    let mut rbuf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 64 * 1024];
    let mut head_end: Option<usize> = None;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut status: u16 = 0;
    let mut resp_headers: Vec<(String, String)> = Vec::new();
    loop {
        if Instant::now() > deadline {
            return Err("request deadline exceeded".into());
        }
        match wire.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                rbuf.extend_from_slice(&tmp[..n]);
                if rbuf.len() > 8 * 1024 * 1024 {
                    return Err("response too large".into());
                }
                if head_end.is_none() {
                    if let Some(pos) = rbuf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = Some(pos + 4);
                        let head_text = String::from_utf8_lossy(&rbuf[..pos]).to_string();
                        let mut lines = head_text.split("\r\n");
                        status = lines
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|s| s.parse().ok())
                            .ok_or("bad status line")?;
                        for line in lines {
                            let Some((k, v)) = line.split_once(':') else { continue };
                            let k = k.trim().to_ascii_lowercase();
                            let v = v.trim();
                            if k == "content-length" {
                                content_length = v.parse().ok();
                            }
                            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                                chunked = true;
                            }
                            resp_headers.push((k, v.to_string()));
                        }
                    }
                }
                if let (Some(he), Some(cl)) = (head_end, content_length) {
                    if rbuf.len() >= he + cl {
                        break;
                    }
                }
                if chunked && head_end.is_some() && rbuf.ends_with(b"0\r\n\r\n") {
                    break;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err("read timeout".into());
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if head_end.is_some() && content_length.is_none() && !chunked => {
                let _ = e; // close-notify omission ends the body
                break;
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let he = head_end.ok_or("response ended before headers completed")?;
    let raw = &rbuf[he..];
    let body = if chunked { dechunk(raw)? } else { raw.to_vec() };
    if let Some(cl) = content_length {
        if body.len() < cl {
            return Err(format!("short body: {} of {cl} bytes", body.len()));
        }
    }
    Ok((status, body, resp_headers))
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let pos = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("chunked: missing size line")?;
        let size_line = std::str::from_utf8(&raw[..pos]).map_err(|_| "chunked: bad size")?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| "chunked: bad size hex")?;
        raw = &raw[pos + 2..];
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < size + 2 {
            return Err("chunked: truncated".into());
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = Url::parse("https://delegated-ipfs.dev").unwrap();
        assert!(u.https);
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("delegated-ipfs.dev", 443, "/"));
        let u = Url::parse("http://127.0.0.1:15802/routing/v1/ipns/k51").unwrap();
        assert!(!u.https);
        assert_eq!((u.host.as_str(), u.port), ("127.0.0.1", 15802));
        assert_eq!(u.path, "/routing/v1/ipns/k51");
        assert_eq!(u.origin(), "http://127.0.0.1:15802");
        assert!(Url::parse("ftp://x").is_err());
    }
}
