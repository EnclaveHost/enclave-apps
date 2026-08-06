// Client for the RISC Box app's HTTP API — the other half of the bridge.
//
// We pull frames from GET /fb.rgb (raw RGB, the encoder's input) and push
// remote input into POST /hid, which lands on the emulator's virtio-input
// device. A tiny hand-rolled HTTP/1.1 client keeps the dependency set at
// openssl only.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct App {
    /// host:port of the RISC Box app, e.g. "127.0.0.1:8000".
    addr: String,
}

impl App {
    pub fn new(base: &str) -> App {
        // Accept either "host:port" or "http://host:port".
        let addr = base.trim_start_matches("http://").trim_end_matches('/').to_string();
        App { addr }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn connect(&self) -> std::io::Result<TcpStream> {
        let s = TcpStream::connect(&self.addr)?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(s)
    }

    /// GET a path and return the response body.
    pub fn get(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let mut s = self.connect()?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: */*\r\n\r\n",
            self.addr
        );
        s.write_all(req.as_bytes())?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        Ok(split_body(raw))
    }

    /// POST a JSON body, ignoring the response.
    pub fn post_json(&self, path: &str, body: &str) -> std::io::Result<()> {
        let mut s = self.connect()?;
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
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
