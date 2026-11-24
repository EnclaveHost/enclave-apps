//! Minimal async HTTP/1.1 for an S3 endpoint: keep-alive, Content-Length
//! request bodies (bounded), `Expect: 100-continue`, and response heads with
//! either a buffered body or one the caller streams after the head.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEAD: usize = 64 << 10;
/// A single PUT larger than the buffered body limit streams to Sia (risc-box saves a ~1 GB disk in one PUT); up to this.
pub const MAX_STREAM: u64 = 16 << 30;

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
    /// a streamed PUT: `body` holds the bytes already read, and this many more are still on the socket
    pub stream: Option<u64>,
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

    let chunked = match get("transfer-encoding") {
        None => false,
        Some(v) if v.eq_ignore_ascii_case("identity") => false,
        // wasi:http clients (jot, depot) send even an empty DELETE body chunked, and R2 accepts it
        Some(v) if v.eq_ignore_ascii_case("chunked") => true,
        Some(_) => return Err(ReadError::Bad("unsupported transfer-encoding".into())),
    };
    if chunked {
        let keep_alive = match get("connection") {
            Some(v) if v.eq_ignore_ascii_case("close") => false,
            Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
            _ => !http10,
        };
        if get("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) && buf.len() == head_end {
            s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.map_err(|e| ReadError::Bad(e.to_string()))?;
        }
        let mut rest = buf.split_off(head_end);
        let body = read_chunked(s, &mut rest, max_body).await?;
        *carry = rest;
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target, String::new()),
        };
        return Ok(Request { method, path, query, headers, body, keep_alive, stream: None });
    }
    let len: usize = match get("content-length") {
        Some(v) => v.parse().map_err(|_| ReadError::Bad("bad content-length".into()))?,
        None => 0,
    };
    // only a plain object PUT may stream (not a part, not a copy); everything else stays bounded
    let streams = len > max_body
        && method == "PUT"
        && !target.contains("uploadId=")
        && get("x-amz-copy-source").is_none()
        && len as u64 <= MAX_STREAM;
    if len > max_body && !streams {
        return Err(ReadError::TooLarge);
    }
    let keep_alive = streams.then_some(false).unwrap_or(match get("connection") {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        _ => !http10,
    });
    if len > 0 && get("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) && buf.len() == head_end {
        s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await.map_err(|e| ReadError::Bad(e.to_string()))?;
    }

    let mut body = buf.split_off(head_end);
    if streams {
        body.truncate(len);
        let rest = (len - body.len()) as u64;
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target, String::new()),
        };
        return Ok(Request { method, path, query, headers, body, keep_alive, stream: Some(rest) });
    }
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
    Ok(Request { method, path, query, headers, body, keep_alive, stream: None })
}

/// A chunked body, at most `max_body` bytes; `buf` holds bytes already read and keeps any read past the body's end.
async fn read_chunked(s: &mut TcpStream, buf: &mut Vec<u8>, max_body: usize) -> Result<Vec<u8>, ReadError> {
    async fn more(s: &mut TcpStream, buf: &mut Vec<u8>) -> Result<(), ReadError> {
        let mut chunk = [0u8; 16 << 10];
        let n = s.read(&mut chunk).await.map_err(|e| ReadError::Bad(e.to_string()))?;
        if n == 0 {
            return Err(ReadError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }
    async fn line(s: &mut TcpStream, buf: &mut Vec<u8>) -> Result<String, ReadError> {
        loop {
            if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                let l = String::from_utf8_lossy(&buf[..i]).to_string();
                buf.drain(..i + 2);
                return Ok(l);
            }
            if buf.len() > 8 << 10 {
                return Err(ReadError::Bad("chunk size line too long".into()));
            }
            more(s, buf).await?;
        }
    }
    let mut body = Vec::new();
    loop {
        let size_line = line(s, buf).await?;
        let hex = size_line.split(';').next().unwrap_or("").trim();
        let n = usize::from_str_radix(hex, 16).map_err(|_| ReadError::Bad("bad chunk size".into()))?;
        if n == 0 {
            // trailers, then the blank line that ends the body
            while !line(s, buf).await?.is_empty() {}
            return Ok(body);
        }
        if body.len() + n > max_body {
            return Err(ReadError::TooLarge);
        }
        while buf.len() < n + 2 {
            more(s, buf).await?;
        }
        if &buf[n..n + 2] != b"\r\n" {
            return Err(ReadError::Bad("chunk not followed by CRLF".into()));
        }
        body.extend_from_slice(&buf[..n]);
        buf.drain(..n + 2);
    }
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
