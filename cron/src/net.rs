//! Same socket/TLS path on native and WASI. Only configured targets are dialled;
//! redirects are never followed. Polling keeps long agent runs off the API loop.
use std::{
    io::{ErrorKind, Read, Write},
    net::TcpStream,
    sync::Arc,
    time::{Duration, Instant},
};
use url::Url;

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl Reply {
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
            Self::Plain(s) => s.read(b),
            Self::Tls(s) => s.read(b),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(b),
            Self::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}
pub struct Pending {
    wire: Wire,
    send: Vec<u8>,
    sent: usize,
    recv: Vec<u8>,
    max: usize,
    deadline: Instant,
    eof: bool,
}
pub fn safe_url(raw: &str, dev: bool) -> Result<Url, String> {
    let u = Url::parse(raw).map_err(|_| "Invalid target URL")?;
    if !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
        || u.host_str().is_none()
    {
        return Err("URL must have a host and no credentials or fragment".into());
    }
    if u.scheme() != "https"
        && !(dev
            && u.scheme() == "http"
            && matches!(u.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")))
    {
        return Err("HTTPS required (local test mode permits loopback HTTP only)".into());
    }
    Ok(u)
}
impl Pending {
    pub fn start(
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        seconds: u64,
        max: usize,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let u = Url::parse(url).map_err(|_| "Invalid URL")?;
        let host = u.host_str().ok_or("Missing host")?.trim_matches(['[', ']']);
        let port = u.port_or_known_default().ok_or("Missing port")?;
        let sock = crate::egress::dial(host, port, None).map_err(|_| "Target connection failed")?;
        sock.set_nonblocking(true)
            .map_err(|_| "Cannot make socket nonblocking")?;
        let wire = if u.scheme() == "https" {
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls_rustcrypto::provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| "TLS setup failed")?
            .with_root_certificates(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            })
            .with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|_| "Bad TLS name")?;
            let c = rustls::ClientConnection::new(Arc::new(cfg), name)
                .map_err(|_| "TLS setup failed")?;
            Wire::Tls(Box::new(rustls::StreamOwned::new(c, sock)))
        } else {
            Wire::Plain(sock)
        };
        if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err("Unsupported method".into());
        }
        let authority = &u[url::Position::BeforeHost..url::Position::AfterPort];
        let path = &u[url::Position::BeforePath..url::Position::AfterQuery];
        let mut send=format!("{method} {path} HTTP/1.1\r\nhost: {authority}\r\nconnection: close\r\ncontent-length: {}\r\n",body.len());
        for (k, v) in headers {
            if k.is_empty()
                || !k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                || v.contains(['\r', '\n'])
                || matches!(
                    k.to_ascii_lowercase().as_str(),
                    "host" | "connection" | "content-length" | "transfer-encoding"
                )
            {
                return Err("Invalid configured header".into());
            }
            send.push_str(&format!("{k}: {v}\r\n"));
        }
        send.push_str("\r\n");
        let mut send = send.into_bytes();
        send.extend_from_slice(body);
        Ok(Self {
            wire,
            send,
            sent: 0,
            recv: Vec::new(),
            max,
            deadline,
            eof: false,
        })
    }
    pub fn poll(&mut self) -> Option<Result<Reply, String>> {
        if Instant::now() >= self.deadline {
            return Some(Err(
                "Target deadline exceeded; delivery may have happened".into()
            ));
        }
        for _ in 0..16 {
            if self.sent < self.send.len() {
                match self.wire.write(&self.send[self.sent..]) {
                    Ok(0) => return Some(Err("Target closed while sending".into())),
                    Ok(n) => self.sent += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => return None,
                    Err(_) => return Some(Err("Target write/TLS failure".into())),
                };
                continue;
            }
            match self.wire.flush() {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => return None,
                Err(_) => return Some(Err("Target TLS flush failed".into())),
            }
            let mut b = [0; 16384];
            match self.wire.read(&mut b) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    self.recv.extend_from_slice(&b[..n]);
                    match parse(&self.recv, false, self.max) {
                        Ok(Some(reply)) => return Some(Ok(reply)),
                        Err(e) => return Some(Err(e)),
                        Ok(None) => {}
                    }
                    if self.recv.len() > self.max.saturating_mul(2) + 32768 {
                        return Some(Err("Target response exceeds limit".into()));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return Some(Err("Target read/TLS failure".into())),
            }
        }
        match parse(&self.recv, self.eof, self.max) {
            Ok(Some(r)) => Some(Ok(r)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
    pub fn wait(mut self) -> Result<Reply, String> {
        loop {
            if let Some(r) = self.poll() {
                return r;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
fn parse(b: &[u8], eof: bool, max: usize) -> Result<Option<Reply>, String> {
    let mut hs = [httparse::EMPTY_HEADER; 64];
    let mut r = httparse::Response::new(&mut hs);
    let offset = match r.parse(b).map_err(|_| "Malformed HTTP response")? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => {
            if eof || b.len() > 16384 {
                return Err("Incomplete/oversized response headers".into());
            }
            return Ok(None);
        }
    };
    if offset > 16384 {
        return Err("Oversized response headers".into());
    }
    let status = r.code.ok_or("Missing HTTP status")?;
    let headers: Vec<_> = r
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).trim().to_string(),
            )
        })
        .collect();
    let lengths: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k == "content-length")
        .collect();
    let tes: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k == "transfer-encoding")
        .collect();
    if lengths.len() > 1 || tes.len() > 1 || (!lengths.is_empty() && !tes.is_empty()) {
        return Err("Ambiguous HTTP framing".into());
    }
    let raw = &b[offset..];
    let body = if status == 204 || status == 304 {
        Some(Vec::new())
    } else if let Some((_, te)) = tes.first() {
        if !te.eq_ignore_ascii_case("chunked") {
            return Err("Unsupported transfer encoding".into());
        }
        dechunk(raw, max)?
    } else if let Some((_, n)) = lengths.first() {
        let n: usize = n.parse().map_err(|_| "Bad content length")?;
        if n > max {
            return Err("Response too large".into());
        }
        if raw.len() >= n {
            Some(raw[..n].to_vec())
        } else {
            None
        }
    } else {
        if raw.len() > max {
            return Err("Response too large".into());
        }
        if eof {
            Some(raw.to_vec())
        } else {
            None
        }
    };
    match body {
        Some(body) => Ok(Some(Reply {
            status,
            headers,
            body,
        })),
        None if eof => Err("Truncated HTTP body".into()),
        None => Ok(None),
    }
}
fn dechunk(mut b: &[u8], max: usize) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::new();
    loop {
        let Some(i) = b.windows(2).position(|s| s == b"\r\n") else {
            return Ok(None);
        };
        if i > 128 {
            return Err("Chunk size line too long".into());
        }
        let line = std::str::from_utf8(&b[..i]).map_err(|_| "Bad chunk")?;
        let n = usize::from_str_radix(line.split(';').next().unwrap_or(""), 16)
            .map_err(|_| "Bad chunk length")?;
        b = &b[i + 2..];
        if n == 0 {
            return if b.starts_with(b"\r\n") || b.windows(4).any(|w| w == b"\r\n\r\n") {
                Ok(Some(out))
            } else {
                Ok(None)
            };
        }
        if n > max.saturating_sub(out.len()) {
            return Err("Response too large".into());
        }
        if b.len() < n + 2 {
            return Ok(None);
        }
        if &b[n..n + 2] != b"\r\n" {
            return Err("Bad chunk terminator".into());
        }
        out.extend_from_slice(&b[..n]);
        b = &b[n + 2..];
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing() {
        assert_eq!(
            parse(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
                false,
                8
            )
            .unwrap()
            .unwrap()
            .body,
            b"abc"
        );
        assert!(parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc",
            true,
            20
        )
        .is_err());
        assert!(parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            false,
            20
        )
        .is_err());
    }
    #[test]
    fn urls() {
        assert!(safe_url("https://good.example/run", false).is_ok());
        for u in [
            "http://bad.example/",
            "https://user:pass@bad.example/",
            "https://a.example/#frag",
            "file:///tmp/a",
        ] {
            assert!(safe_url(u, false).is_err());
        }
    }
}
