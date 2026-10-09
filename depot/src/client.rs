//! A blocking HTTP/1.1 client that keeps its connection: one per storage
//! endpoint, TLS via rustls (pure-Rust RustCrypto provider, webpki roots),
//! dialled through the platform's egress front when there is one.
//!
//! A reused connection the peer has quietly closed shows up as a failure
//! before any response byte; that request is sent once more on a fresh
//! connection, which is what every keep-alive client does.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_HEAD: usize = 64 * 1024;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, n: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    }
}

enum Wire {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(b),
            Wire::Tls(s) => s.read(b),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(b),
            Wire::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}

pub struct Client {
    pub https: bool,
    pub host: String,
    pub port: u16,
    tls: Option<Arc<rustls::ClientConfig>>,
    wire: Option<Wire>,
    pub connects: u64,
    pub requests: u64,
    pub max_body: usize,
    pub timeout: Duration,
}

/// Parse `http(s)://host[:port]` (no path).
pub fn parse_origin(url: &str) -> Result<(bool, String, u16), String> {
    let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!("endpoint must be http(s)://host, got {url}"));
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') || rest.contains('@') || rest.contains('?') {
        return Err("endpoint must be scheme://host[:port] with no path".into());
    }
    let default = if https { 443 } else { 80 };
    let bad_port = |_| "bad port in endpoint".to_string();
    let (host, port) = if let Some(r) = rest.strip_prefix('[') {
        let (h, tail) = r.split_once(']').ok_or("bad IPv6 endpoint")?;
        match tail.strip_prefix(':') {
            Some(p) => (h.to_string(), p.parse().map_err(bad_port)?),
            None if tail.is_empty() => (h.to_string(), default),
            None => return Err("bad IPv6 endpoint".into()),
        }
    } else if let Some((h, p)) = rest.rsplit_once(':') {
        (h.to_string(), p.parse().map_err(bad_port)?)
    } else {
        (rest.to_string(), default)
    };
    Ok((https, host, port))
}

impl Client {
    pub fn new(origin: &str) -> Result<Client, String> {
        let (https, host, port) = parse_origin(origin)?;
        let tls = if https {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls_rustcrypto::provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls versions: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Some(Arc::new(cfg))
        } else {
            None
        };
        Ok(Client {
            https,
            host,
            port,
            tls,
            wire: None,
            connects: 0,
            requests: 0,
            max_body: 96 << 20,
            timeout: IO_TIMEOUT,
        })
    }

    /// The Host header value.
    pub fn authority(&self) -> String {
        let default = if self.https { 443 } else { 80 };
        let h = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            h
        } else {
            format!("{h}:{}", self.port)
        }
    }

    fn connect(&mut self) -> Result<Wire, String> {
        self.connects += 1;
        let sock = crate::egress::dial_bounded(&self.host, self.port, self.timeout)?;
        let _ = sock.set_read_timeout(Some(self.timeout));
        let _ = sock.set_write_timeout(Some(self.timeout));
        let _ = sock.set_nodelay(true);
        match &self.tls {
            None => Ok(Wire::Plain(sock)),
            Some(cfg) => {
                let name = rustls::pki_types::ServerName::try_from(self.host.clone())
                    .map_err(|_| format!("bad TLS server name {}", self.host))?;
                let c = rustls::ClientConnection::new(cfg.clone(), name)
                    .map_err(|e| format!("tls setup: {e}"))?;
                Ok(Wire::Tls(Box::new(rustls::StreamOwned::new(c, sock))))
            }
        }
    }

    pub fn drop_connection(&mut self) {
        self.wire = None;
    }

    /// Send one request. A request that is not idempotent (a conditional
    /// write) is never sent twice: when its outcome is unknown the error says
    /// so, and the caller reads back what the store holds.
    pub fn send(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response, String> {
        self.send_once(method, target, headers, body, true)
    }

    pub fn send_once(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: &[u8],
        resend_on_stale: bool,
    ) -> Result<Response, String> {
        self.requests += 1;
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nhost: {}\r\n",
            self.authority()
        );
        for (k, v) in headers {
            if v.contains(['\r', '\n']) || k.contains(['\r', '\n', ':']) {
                return Err("invalid request header".into());
            }
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
        for attempt in 0..2 {
            let reused = self.wire.is_some();
            let mut wire = match self.wire.take() {
                Some(w) => w,
                None => self.connect()?,
            };
            match exchange(&mut wire, method, head.as_bytes(), body, self.max_body) {
                Ok((resp, keep)) => {
                    if keep {
                        self.wire = Some(wire);
                    }
                    return Ok(resp);
                }
                Err((e, got_bytes)) => {
                    if resend_on_stale && reused && !got_bytes && attempt == 0 {
                        continue; // the idle connection was gone: once more, fresh
                    }
                    return Err(e);
                }
            }
        }
        Err("request failed".into())
    }
}

type ExchangeErr = (String, bool);

fn exchange(
    w: &mut Wire,
    method: &str,
    head: &[u8],
    body: &[u8],
    max_body: usize,
) -> Result<(Response, bool), ExchangeErr> {
    let io = |e: std::io::Error| (format!("storage connection: {e}"), false);
    w.write_all(head).map_err(io)?;
    for ch in body.chunks(256 * 1024) {
        w.write_all(ch).map_err(io)?;
    }
    w.flush().map_err(io)?;
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut tmp = vec![0u8; 64 * 1024];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|x| x == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(("storage response headers too large".into(), true));
        }
        match w.read(&mut tmp) {
            Ok(0) => return Err(("storage closed the connection".into(), !buf.is_empty())),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err((format!("storage read: {e}"), !buf.is_empty())),
        }
    };
    let text = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ("bad storage status line".to_string(), true))?;
    let mut headers = Vec::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let mut keep = !get("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"))
        && status_line.starts_with("HTTP/1.1");
    let mut rest = buf[head_end..].to_vec();
    let no_body =
        method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status);
    let body = if no_body {
        Vec::new()
    } else if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        read_chunked(w, rest, max_body, &mut tmp)?
    } else if let Some(cl) = get("content-length") {
        let n: usize = cl
            .parse()
            .map_err(|_| ("bad content-length from storage".to_string(), true))?;
        if n > max_body {
            return Err(("storage response too large".into(), true));
        }
        while rest.len() < n {
            match w.read(&mut tmp) {
                Ok(0) => return Err(("storage response truncated".into(), true)),
                Ok(k) => rest.extend_from_slice(&tmp[..k]),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                // R2 sometimes closes without TLS close_notify; a short body is still caught above
                Err(e) => return Err((format!("storage read: {e}"), true)),
            }
        }
        if rest.len() > n {
            keep = false;
        }
        rest.truncate(n);
        rest
    } else {
        keep = false;
        loop {
            match w.read(&mut tmp) {
                Ok(0) => break,
                Ok(k) => {
                    rest.extend_from_slice(&tmp[..k]);
                    if rest.len() > max_body {
                        return Err(("storage response too large".into(), true));
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => return Err((format!("storage read: {e}"), true)),
            }
        }
        rest
    };
    Ok((
        Response {
            status,
            headers,
            body,
        },
        keep,
    ))
}

fn read_chunked(
    w: &mut Wire,
    mut buf: Vec<u8>,
    max: usize,
    tmp: &mut [u8],
) -> Result<Vec<u8>, ExchangeErr> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut fill = |buf: &mut Vec<u8>, w: &mut Wire| -> Result<(), ExchangeErr> {
        loop {
            match w.read(tmp) {
                Ok(0) => return Err(("storage chunked body truncated".into(), true)),
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err((format!("storage read: {e}"), true)),
            }
        }
    };
    loop {
        let line_end = loop {
            if let Some(p) = buf[pos..].windows(2).position(|x| x == b"\r\n") {
                break pos + p;
            }
            if buf.len() - pos > 1024 {
                return Err(("bad chunk size line from storage".into(), true));
            }
            fill(&mut buf, w)?;
        };
        let line = String::from_utf8_lossy(&buf[pos..line_end]).to_string();
        let n = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| ("bad chunk size from storage".to_string(), true))?;
        pos = line_end + 2;
        if n == 0 {
            // trailers until the blank line
            loop {
                if let Some(p) = buf[pos..].windows(2).position(|x| x == b"\r\n") {
                    if p == 0 {
                        return Ok(out);
                    }
                    pos += p + 2;
                    continue;
                }
                fill(&mut buf, w)?;
            }
        }
        if n > max || out.len() > max - n {
            return Err(("storage response too large".into(), true));
        }
        let need = pos
            .checked_add(n)
            .and_then(|x| x.checked_add(2))
            .ok_or_else(|| ("bad chunk size from storage".to_string(), true))?;
        while buf.len() < need {
            fill(&mut buf, w)?;
        }
        out.extend_from_slice(&buf[pos..pos + n]);
        pos += n + 2;
        if pos > 1 << 20 {
            buf.drain(..pos);
            pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(
            parse_origin("https://x.r2.cloudflarestorage.com").unwrap(),
            (true, "x.r2.cloudflarestorage.com".into(), 443)
        );
        assert_eq!(
            parse_origin("http://127.0.0.1:9000/").unwrap(),
            (false, "127.0.0.1".into(), 9000)
        );
        assert_eq!(
            parse_origin("http://[::1]:9000").unwrap(),
            (false, "::1".into(), 9000)
        );
        assert!(parse_origin("https://a/b").is_err());
        assert!(parse_origin("ftp://a").is_err());
        assert!(parse_origin("https://u@a").is_err());
    }
}
