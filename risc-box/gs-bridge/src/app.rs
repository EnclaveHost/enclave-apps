// Client for the RISC Box app's HTTP API — the other half of the bridge.
//
// We pull frames from GET /fb.rgb (raw RGB, the encoder's input) and push
// remote input into POST /hid, which lands on the emulator's virtio-input
// device. A tiny hand-rolled HTTP/1.1 client keeps the dependency set at
// openssl only.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use openssl::ssl::{SslConnector, SslMethod};

/// Either a plain socket or a TLS one. A deployment on the fleet terminates
/// TLS inside the enclave and serves nothing in the clear, so reaching one at
/// all means speaking https; a local `wasmtime run` serves plain http. Both
/// are just a Read + Write to everything above this.
enum Conn {
    Plain(TcpStream),
    Tls(Box<openssl::ssl::SslStream<TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

pub struct App {
    /// host:port of the RISC Box app, e.g. "127.0.0.1:8000".
    addr: String,
    /// Hostname without the port — TLS needs it for SNI and cert validation,
    /// and it is what belongs in the Host header.
    host: String,
    tls: bool,
    /// Bearer token for a deployment whose config sets `api_key`.
    api_key: Option<String>,
}

impl App {
    /// Accepts "host:port", "http://host[:port]" or "https://host[:port]".
    /// https defaults to port 443, http to the port given (or 80).
    pub fn new(base: &str) -> App {
        let tls = base.starts_with("https://");
        let rest = base
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let host = rest.split(':').next().unwrap_or(rest).to_string();
        let addr = match rest.contains(':') {
            true => rest.to_string(),
            false => format!("{rest}:{}", if tls { 443 } else { 80 }),
        };
        App { addr, host, tls, api_key: None }
    }

    /// Set the bearer token sent with every request.
    pub fn with_api_key(mut self, key: Option<String>) -> App {
        self.api_key = key;
        self
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn auth_header(&self) -> String {
        match &self.api_key {
            Some(k) => format!("Authorization: Bearer {k}\r\n"),
            None => String::new(),
        }
    }

    fn connect(&self) -> std::io::Result<Conn> {
        let s = TcpStream::connect(&self.addr)?;
        s.set_nodelay(true)?;
        // Generous: a frame read crosses the internet on the remote path, and
        // the app's event loop can be mid-instruction-batch when it arrives.
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        s.set_write_timeout(Some(Duration::from_secs(30)))?;
        if !self.tls {
            return Ok(Conn::Plain(s));
        }
        let connector = SslConnector::builder(SslMethod::tls_client())
            .map_err(|e| std::io::Error::other(format!("tls setup: {e}")))?
            .build();
        let stream = connector
            .connect(&self.host, s)
            .map_err(|e| std::io::Error::other(format!("tls handshake with {}: {e}", self.host)))?;
        Ok(Conn::Tls(Box::new(stream)))
    }

    /// GET a path and return the response body.
    pub fn get(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let mut s = self.connect()?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{}Connection: close\r\nAccept: */*\r\n\r\n",
            self.host,
            self.auth_header()
        );
        s.write_all(req.as_bytes())?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        Ok(split_body(raw))
    }

    /// Open a streaming GET and hand back the connection positioned just after
    /// the response headers. Used for the long-lived /display event stream,
    /// where `get()` is useless — it reads to EOF and the stream never ends.
    pub fn get_stream(&self, path: &str) -> std::io::Result<impl std::io::BufRead> {
        let mut s = self.connect()?;
        // No read timeout: an idle screen sends nothing for as long as it
        // stays idle, and that is not an error.
        if let Conn::Plain(t) = &s {
            t.set_read_timeout(None)?;
        }
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{}Accept: text/event-stream\r\n\r\n",
            self.host,
            self.auth_header()
        );
        s.write_all(req.as_bytes())?;
        let mut r = std::io::BufReader::new(s);
        // Consume the status line and headers.
        let mut line = String::new();
        loop {
            line.clear();
            if std::io::BufRead::read_line(&mut r, &mut line)? == 0 {
                return Err(std::io::Error::other("stream closed during headers"));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
        }
        Ok(r)
    }

    /// POST a JSON body, ignoring the response.
    pub fn post_json(&self, path: &str, body: &str) -> std::io::Result<()> {
        let mut s = self.connect()?;
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\n{}Content-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.host,
            self.auth_header(),
            body.len()
        );
        s.write_all(req.as_bytes())?;
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
        Ok(())
    }
}

/// Strip HTTP headers (and de-chunk if needed) from a raw response.
fn split_body(raw: Vec<u8>) -> Vec<u8> {
    let Some(hdr_end) = find(&raw, b"\r\n\r\n") else {
        return Vec::new();
    };
    let head = String::from_utf8_lossy(&raw[..hdr_end]).to_ascii_lowercase();
    let body = raw[hdr_end + 4..].to_vec();

    if head.contains("transfer-encoding: chunked") {
        dechunk(&body)
    } else {
        body
    }
}

fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        let Some(eol) = find(&body[i..], b"\r\n") else { break };
        let size_str = String::from_utf8_lossy(&body[i..i + eol]).to_string();
        let size_str = size_str.split(';').next().unwrap_or("").trim().to_string();
        let Ok(size) = usize::from_str_radix(&size_str, 16) else { break };
        i += eol + 2;
        if size == 0 {
            break;
        }
        if i + size > body.len() {
            out.extend_from_slice(&body[i..]);
            break;
        }
        out.extend_from_slice(&body[i..i + size]);
        i += size + 2; // skip the chunk's trailing CRLF
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
