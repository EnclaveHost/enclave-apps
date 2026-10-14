//! Minimal async HTTP/1.1 for an S3 endpoint: keep-alive, Content-Length
//! request bodies (bounded), `Expect: 100-continue`, and response heads with
//! either a buffered body or one the caller streams after the head.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEAD: usize = 64 << 10;

pub struct Request {
    pub method: String,
    /// path exactly as sent (still percent-encoded)
    pub path: String,
    /// raw query string, without '?'
    pub query: String,
    /// names lowercased
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub enum ReadError {
    Closed,
    TooLarge,
    Bad(String),
}

/// Reads one request. `carry` holds bytes already read past the previous request.
pub async fn read_request(s: &mut TcpStream, carry: &mut Vec<u8>, max_body: usize) -> Result<Request, ReadError> {
    let mut buf = std::mem::take(carry);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(ReadError::Bad("request head too large".into()));
        }
        let mut chunk = [0u8; 16 << 10];
        let n = s.read(&mut chunk).await.map_err(|e| ReadError::Bad(e.to_string()))?;
        if n == 0 {
            return Err(ReadError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let mut hdrs = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut hdrs);
    match req.parse(&buf[..head_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(ReadError::Bad("malformed request head".into())),
    }
    let method = req.method.unwrap_or("").to_string();
    let target = req.path.unwrap_or("/").to_string();
    let http10 = req.version == Some(0);
    let headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).trim().to_string()))
        .collect();
    let get = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());

    if get("transfer-encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Err(ReadError::Bad("chunked request bodies are not supported; send Content-Length".into()));
    }
    let len: usize = match get("content-length") {
        Some(v) => v.parse().map_err(|_| ReadError::Bad("bad content-length".into()))?,
        None => 0,
    };
    if len > max_body {
        return Err(ReadError::TooLarge);
    }
    let keep_alive = match get("connection") {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        _ => !http10,
    };
    if len > 0 && get("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) && buf.len() == head_end {
        s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.map_err(|e| ReadError::Bad(e.to_string()))?;
    }

    let mut body = buf.split_off(head_end);
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(256 << 10)];
        let n = s.read(&mut chunk).await.map_err(|e| ReadError::Bad(e.to_string()))?;
        if n == 0 {
            return Err(ReadError::Closed);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if body.len() > len {
        *carry = body.split_off(len);
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Ok(Request { method, path, query, headers, body, keep_alive })
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        304 => "Not Modified",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Writes a response head; the body (of `len` bytes) is the caller's to write next.
pub async fn write_head(s: &mut TcpStream, status: u16, headers: &[(String, String)], len: u64, keep_alive: bool) -> std::io::Result<()> {
    let mut h = format!("HTTP/1.1 {status} {}\r\nServer: sia-s3\r\nContent-Length: {len}\r\n", reason(status));
    for (k, v) in headers {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str(if keep_alive { "Connection: keep-alive\r\n\r\n" } else { "Connection: close\r\n\r\n" });
    s.write_all(h.as_bytes()).await
}
