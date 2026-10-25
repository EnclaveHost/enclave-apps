use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

const HTML: &str = include_str!("index.html");
fn read_headers(s: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    s.set_nonblocking(true)?;
    let mut b = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let mut part = [0; 2048];
        match s.read(&mut part) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                b.extend_from_slice(&part[..n]);
                if b.len() > 16384 {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
                if b.windows(4).any(|x| x == b"\r\n\r\n") {
                    return Ok(b);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        if Instant::now() > until {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn send(s: &mut TcpStream, status: u16, mime: &str, body: &[u8]) {
    let auth = if status == 401 {
        "WWW-Authenticate: Basic realm=\"KVM Box\"\r\n"
    } else {
        ""
    };
    let header=format!("HTTP/1.1 {status} Response\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\n{auth}\r\n",body.len());
    let mut data = header.into_bytes();
    data.extend_from_slice(body);
    let until = Instant::now() + Duration::from_secs(3);
    let mut at = 0;
    while at < data.len() && Instant::now() < until {
        match s.write(&data[at..]) {
            Ok(0) => break,
            Ok(n) => at += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(_) => break,
        }
    }
}
fn authorized(value: &str, wanted: &str) -> bool {
    let Some(b) = value
        .strip_prefix("Basic ")
        .and_then(|v| STANDARD.decode(v).ok())
    else {
        return false;
    };
    let Some(password) = b.strip_prefix(b"owner:") else {
        return false;
    };
    let actual = format!("{:x}", Sha256::digest(password));
    actual.len() == wanted.len()
        && actual
            .bytes()
            .zip(wanted.bytes())
            .fold(0u8, |a, (x, y)| a | (x ^ y))
            == 0
}
fn broker(method: &str, path: &str) -> Result<Vec<u8>, String> {
    let mut s = TcpStream::connect("127.0.0.1:19080").map_err(|e| e.to_string())?;
    s.write_all(format!("{method} /v1/{path} HTTP/1.1\r\nHost: broker\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).map_err(|e|e.to_string())?;
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    let until = Instant::now() + Duration::from_secs(5);
    let mut out = Vec::new();
    loop {
        let mut b = [0; 4096];
        match s.read(&mut b) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&b[..n]);
                if out.len() > 32768 {
                    return Err("broker response too large".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() > until {
            return Err("broker timeout".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let p = out
        .windows(4)
        .position(|b| b == b"\r\n\r\n")
        .ok_or("invalid broker response")?;
    if !out.starts_with(b"HTTP/1.1 200 ")
        && !out.starts_with(b"HTTP/1.0 200 ")
        && !out.starts_with(b"HTTP/1.1 202 ")
        && !out.starts_with(b"HTTP/1.0 202 ")
    {
        return Err("broker refused request".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&out[p + 4..]).map_err(|e| e.to_string())?;
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}
fn handle(mut s: TcpStream, wanted: &str) {
    let Ok(b) = read_headers(&mut s) else { return };
    let mut hs = [httparse::EMPTY_HEADER; 64];
    let mut r = httparse::Request::new(&mut hs);
    let Ok(httparse::Status::Complete(n)) = r.parse(&b) else {
        send(&mut s, 400, "text/plain", b"Bad request");
        return;
    };
    let header = |name: &str| {
        r.headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .collect::<Vec<_>>()
    };
    let auth = header("authorization");
    if auth.len() != 1 || !authorized(std::str::from_utf8(auth[0].value).unwrap_or(""), wanted) {
        send(&mut s, 401, "text/plain", b"Sign in as owner.");
        return;
    }
    if n != b.len()
        || !header("transfer-encoding").is_empty()
        || header("content-length").iter().any(|h| h.value != b"0")
    {
        send(
            &mut s,
            400,
            "text/plain",
            b"Request bodies are not supported",
        );
        return;
    }
    let method = r.method.unwrap_or("");
    let path = r.path.unwrap_or("");
    if method == "GET" && path == "/" {
        send(&mut s, 200, "text/html; charset=utf-8", HTML.as_bytes());
        return;
    }
    let action = match (method, path) {
        ("GET", "/api/status") => "status",
        ("POST", "/api/start") => "start",
        ("POST", "/api/stop") => "stop",
        _ => {
            send(&mut s, 404, "text/plain", b"Not found");
            return;
        }
    };
    if method == "POST"
        && header("x-kvm-box-request").first().map(|h| h.value) != Some(b"1".as_slice())
    {
        send(
            &mut s,
            403,
            "text/plain",
            b"Explicit action header required",
        );
        return;
    }
    match broker(method, action) {
        Ok(body) => send(&mut s, 200, "application/json", &body),
        Err(e) => send(
            &mut s,
            502,
            "application/json",
            serde_json::json!({"error":e}).to_string().as_bytes(),
        ),
    }
}
fn main() {
    let wanted = std::env::var("KVMBOX_ACCESS_SHA256").expect("KVMBOX_ACCESS_SHA256 required");
    assert!(
        wanted.len() == 64
            && wanted
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid access hash"
    );
    let listener = TcpListener::bind("127.0.0.1:8000").expect("listen");
    println!("KVM Box Wasm controller listening on 8000");
    for s in listener.incoming().flatten() {
        handle(s, &wanted)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorization() {
        let hash = format!("{:x}", Sha256::digest(b"secret"));
        assert!(authorized("Basic b3duZXI6c2VjcmV0", &hash));
        assert!(!authorized("Basic b3duZXI6d3Jvbmc=", &hash));
        assert!(!authorized("Basic !!!", &hash));
        assert!(!authorized("Bearer secret", &hash));
    }
}
