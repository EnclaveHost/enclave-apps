//! nanhttpd — a zero-dependency HTTP/1.1 + SSE engine for Enclave service apps.
//!
//! The platform launches run-mode wasm components with `wasmtime run` and a
//! wasi:sockets grant; the deployment's `http:` port is served at its origin
//! (https://<first-8-hex-of-id>.app.enclave.host) by the enclave's in-TEE TLS
//! proxy, which forwards plain HTTP/1.1 to the loopback port we bind. That
//! gives a service app what a `wasi:http` component never gets: one live
//! process for the whole deployment, so state can live in memory.
//!
//! The one platform rule (see network-test): **read `ENCLAVE_PORTS` and bind
//! the actual port, never hardcode.** Entries look like `http:8080=18321`;
//! we prefer the first `http:` entry, fall back to the first entry, and only
//! default to 8080 when the variable is absent (local development).
//!
//! wasm32-wasip2 has no threads, so this is one non-blocking event loop:
//! accept, read/parse/dispatch, flush, reap, then a short sleep. Rust
//! `std::net` maps directly to wasi:sockets on this target — no async
//! runtime, no dependencies.
//!
//! Shape: `Server::poll()` hands the app complete requests; the app answers
//! each with `respond()` or converts the connection into a Server-Sent
//! Events subscriber with `upgrade_sse(topic)`; `broadcast(topic, event)`
//! fans out to every subscriber. SSE frames go out `Transfer-Encoding:
//! chunked` so any HTTP/1.1 hop frames them correctly; the engine emits
//! `:hb` comments every 15s so idle streams and their proxies stay open.

// This file is stamped into each app of the suite unchanged; not every app
// uses every entry point (dead-drop has no SSE, pixelboard no forms).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_TARGET_BYTES: usize = 4 * 1024;
pub const MAX_CONNS: usize = 384;
pub const MAX_WBUF: usize = 512 * 1024; // close a client that can't drain this
const IDLE_KEEPALIVE: Duration = Duration::from_secs(60);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);
// A peer whose write buffer stays wedged this long is gone, whatever its
// socket claims: three missed heartbeats' worth of not draining a byte.
const WRITE_STALL: Duration = Duration::from_secs(45);

pub struct Request {
    pub method: String,
    pub path: String,   // percent-decoded, no query
    pub query: String,  // raw, after '?'
    pub headers: Vec<(String, String)>, // names lowercased
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, reason: &'static str) -> Self {
        Response { status, reason, headers: Vec::new(), body: Vec::new() }
    }
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    pub fn body(mut self, ct: &str, body: impl Into<Vec<u8>>) -> Self {
        self.headers.push(("content-type".into(), ct.into()));
        self.body = body.into();
        self
    }
}

pub fn json(status: u16, reason: &'static str, body: String) -> Response {
    Response::new(status, reason)
        .with("cache-control", "no-store")
        .body("application/json", body)
}

enum ConnState {
    Http { since: Instant, reading_body: bool },
    Sse { topic: String, last_beat: Instant },
    Closing, // flush wbuf, then drop
}

struct Conn {
    stream: TcpStream,
    rbuf: Vec<u8>,
    wbuf: VecDeque<u8>,
    state: ConnState,
    last_activity: Instant,
    keep_alive: bool,
    sent_continue: bool,
    tag: Option<String>,          // app-chosen label for targeted SSE drops
    stuck_since: Option<Instant>, // wbuf continuously non-empty since
}

pub struct Server {
    listener: TcpListener,
    conns: Vec<Conn>,
    app: &'static str,
    started: Instant,
}

/// `ENCLAVE_PORTS=http:8080=18321,tcp:7777=18322` → the actual port to bind.
pub fn resolve_port(default: u16) -> u16 {
    let Ok(ports) = std::env::var("ENCLAVE_PORTS") else { return default };
    let mut first: Option<u16> = None;
    for entry in ports.split(',') {
        let Some((label, actual)) = entry.split_once('=') else { continue };
        let Ok(port) = actual.trim().parse::<u16>() else { continue };
        if first.is_none() {
            first = Some(port);
        }
        if label.trim_start().starts_with("http:") {
            return port;
        }
    }
    first.unwrap_or(default)
}

impl Server {
    pub fn bind(app: &'static str, default_port: u16) -> Server {
        let port = resolve_port(default_port);
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[{app}] fatal: bind 127.0.0.1:{port}: {e}");
                std::process::exit(1);
            }
        };
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        println!("[{app}] listening on 127.0.0.1:{port}");
        Server { listener, conns: Vec::new(), app, started: Instant::now() }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn sse_count(&self, topic: &str) -> usize {
        self.conns
            .iter()
            .filter(|c| matches!(&c.state, ConnState::Sse { topic: t, .. } if t == topic))
            .count()
    }

    /// One pass: accept, read, parse. Returns complete requests as
    /// (conn_key, Request); answer each with respond()/upgrade_sse() before
    /// the next poll (a key is only stable until then).
    pub fn poll(&mut self, max_body: usize) -> Vec<(usize, Request)> {
        // Accept.
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.conns.len() >= MAX_CONNS || stream.set_nonblocking(true).is_err() {
                        continue; // drop on the floor; the proxy will retry
                    }
                    self.conns.push(Conn {
                        stream,
                        rbuf: Vec::new(),
                        wbuf: VecDeque::new(),
                        state: ConnState::Http { since: Instant::now(), reading_body: false },
                        last_activity: Instant::now(),
                        keep_alive: true,
                        sent_continue: false,
                        tag: None,
                        stuck_since: None,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Read and parse.
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        for (i, conn) in self.conns.iter_mut().enumerate() {
            if !matches!(conn.state, ConnState::Http { .. }) {
                // SSE subscribers and closing conns: drain+discard any input.
                loop {
                    match conn.stream.read(&mut buf) {
                        Ok(0) => {
                            conn.state = ConnState::Closing;
                            conn.wbuf.clear();
                            break;
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => {
                            conn.state = ConnState::Closing;
                            conn.wbuf.clear();
                            break;
                        }
                    }
                }
                continue;
            }
            loop {
                match conn.stream.read(&mut buf) {
                    Ok(0) => {
                        conn.keep_alive = false;
                        if conn.rbuf.is_empty() {
                            conn.state = ConnState::Closing;
                        }
                        break;
                    }
                    Ok(n) => {
                        conn.rbuf.extend_from_slice(&buf[..n]);
                        conn.last_activity = Instant::now();
                        if conn.rbuf.len() > MAX_HEADER_BYTES + max_body {
                            overflow(conn, 413, "Payload Too Large");
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        conn.state = ConnState::Closing;
                        conn.wbuf.clear();
                        break;
                    }
                }
            }
            if let ConnState::Http { since, reading_body } = &mut conn.state {
                match try_parse(&mut conn.rbuf, max_body) {
                    Parse::Complete(req) => {
                        if req
                            .header("connection")
                            .map(|v| v.eq_ignore_ascii_case("close"))
                            .unwrap_or(false)
                        {
                            conn.keep_alive = false;
                        }
                        *since = Instant::now();
                        *reading_body = false;
                        conn.sent_continue = false;
                        out.push((i, req));
                    }
                    Parse::Partial { in_body } => {
                        *reading_body = in_body;
                        // curl and friends send `Expect: 100-continue` and
                        // stall a beat waiting for the interim response
                        // before uploading the body — oblige immediately.
                        if in_body && !conn.sent_continue && expects_continue(&conn.rbuf) {
                            conn.sent_continue = true;
                            conn.wbuf.extend(b"HTTP/1.1 100 Continue\r\n\r\n");
                        }
                    }
                    Parse::Bad(status, reason) => overflow(conn, status, reason),
                }
            }
        }
        out
    }

    pub fn respond(&mut self, key: usize, mut resp: Response) {
        let Some(conn) = self.conns.get_mut(key) else { return };
        let keep = conn.keep_alive && resp.status < 500;
        let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
        resp.headers.push(("server".into(), self.app.into()));
        resp.headers
            .push(("content-length".into(), resp.body.len().to_string()));
        resp.headers.push((
            "connection".into(),
            if keep { "keep-alive" } else { "close" }.into(),
        ));
        for (k, v) in &resp.headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        conn.wbuf.extend(head.as_bytes());
        conn.wbuf.extend(&resp.body);
        if !keep {
            conn.state = ConnState::Closing;
        }
    }

    /// Convert the connection into an SSE subscriber on `topic`; `initial`
    /// (already `data: ...\n\n`-framed lines, or "") goes out first.
    pub fn upgrade_sse(&mut self, key: usize, topic: &str, initial: &str) {
        let Some(conn) = self.conns.get_mut(key) else { return };
        let head = format!(
            "HTTP/1.1 200 OK\r\nserver: {}\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nx-accel-buffering: no\r\ntransfer-encoding: chunked\r\nconnection: keep-alive\r\n\r\n",
            self.app
        );
        conn.wbuf.extend(head.as_bytes());
        let hello = format!(": {} stream\n\n{}", self.app, initial);
        chunk_into(&mut conn.wbuf, hello.as_bytes());
        conn.state = ConnState::Sse { topic: topic.into(), last_beat: Instant::now() };
    }

    /// Label an SSE subscriber (call right after upgrade_sse) so a
    /// client-side leave signal can name the exact stream to close: a
    /// browser's `sendBeacon` on pagehide reaches us even when a proxy
    /// hop would happily hold the dead stream's socket open forever.
    pub fn tag_sse(&mut self, key: usize, tag: &str) {
        if let Some(conn) = self.conns.get_mut(key) {
            conn.tag = Some(tag.to_string());
        }
    }

    /// Close every SSE subscriber of `topic` carrying `tag`. Returns how
    /// many were dropped; presence counts correct on the next tick.
    pub fn drop_sse(&mut self, topic: &str, tag: &str) -> usize {
        let mut dropped = 0;
        for conn in &mut self.conns {
            if let ConnState::Sse { topic: t, .. } = &conn.state {
                if t == topic && conn.tag.as_deref() == Some(tag) {
                    conn.state = ConnState::Closing;
                    conn.wbuf.clear();
                    dropped += 1;
                }
            }
        }
        dropped
    }

    /// Send one SSE event (pre-framed body WITHOUT the trailing blank line —
    /// e.g. "event: px\ndata: {...}") to every subscriber of `topic`.
    pub fn broadcast(&mut self, topic: &str, event: &str) {
        let framed = format!("{event}\n\n");
        for conn in &mut self.conns {
            if let ConnState::Sse { topic: t, .. } = &conn.state {
                if t == topic {
                    chunk_into(&mut conn.wbuf, framed.as_bytes());
                }
            }
        }
    }

    /// Flush write buffers, heartbeat SSE, reap dead/expired conns, sleep.
    pub fn flush_and_sleep(&mut self) {
        let busy = self.flush();
        std::thread::sleep(Duration::from_millis(if busy { 2 } else { 25 }));
    }

    /// Like `flush_and_sleep` but without the sleep, for apps whose main
    /// loop has real work to do between polls (risc-box steps a CPU); returns
    /// whether any bytes moved.
    pub fn flush(&mut self) -> bool {
        let now = Instant::now();
        let mut busy = false;
        self.conns.retain_mut(|conn| {
            if let ConnState::Sse { last_beat, .. } = &mut conn.state {
                if now.duration_since(*last_beat) >= SSE_HEARTBEAT {
                    *last_beat = now;
                    chunk_into(&mut conn.wbuf, b":hb\n\n");
                }
            }
            // Flush.
            while !conn.wbuf.is_empty() {
                let (front, _) = conn.wbuf.as_slices();
                match conn.stream.write(front) {
                    Ok(0) => return false,
                    Ok(n) => {
                        conn.wbuf.drain(..n);
                        conn.last_activity = now;
                        busy = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return false,
                }
            }
            if conn.wbuf.len() > MAX_WBUF {
                return false; // slow client
            }
            // Ghost detection: a live peer drains heartbeats within a tick
            // or two; a buffer that stays wedged across three heartbeat
            // intervals marks a connection whose far end has left without
            // saying so. Applies to every state — a mid-response HTTP
            // client that accepts nothing for 45s is equally gone.
            match (conn.wbuf.is_empty(), conn.stuck_since) {
                (true, _) => conn.stuck_since = None,
                (false, None) => conn.stuck_since = Some(now),
                (false, Some(t0)) if now.duration_since(t0) > WRITE_STALL => return false,
                _ => {}
            }
            match &conn.state {
                ConnState::Closing => !conn.wbuf.is_empty(),
                ConnState::Sse { .. } => true,
                ConnState::Http { since, reading_body } => {
                    let idle = now.duration_since(conn.last_activity);
                    if conn.rbuf.is_empty() && !reading_body {
                        idle < IDLE_KEEPALIVE
                    } else {
                        now.duration_since(*since)
                            < if *reading_body { BODY_TIMEOUT } else { HEADER_TIMEOUT }
                    }
                }
            }
        });
        busy
    }
}

fn overflow(conn: &mut Conn, status: u16, reason: &'static str) {
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    conn.wbuf.extend(msg.as_bytes());
    conn.rbuf.clear();
    conn.state = ConnState::Closing;
}

fn chunk_into(wbuf: &mut VecDeque<u8>, data: &[u8]) {
    wbuf.extend(format!("{:x}\r\n", data.len()).as_bytes());
    wbuf.extend(data);
    wbuf.extend(b"\r\n");
}

enum Parse {
    Complete(Request),
    Partial { in_body: bool },
    Bad(u16, &'static str),
}

fn try_parse(rbuf: &mut Vec<u8>, max_body: usize) -> Parse {
    let Some(head_end) = find_crlfcrlf(rbuf) else {
        if rbuf.len() > MAX_HEADER_BYTES {
            return Parse::Bad(431, "Request Header Fields Too Large");
        }
        return Parse::Partial { in_body: false };
    };
    if head_end > MAX_HEADER_BYTES {
        return Parse::Bad(431, "Request Header Fields Too Large");
    }
    let head = match std::str::from_utf8(&rbuf[..head_end]) {
        Ok(s) => s.to_string(),
        Err(_) => return Parse::Bad(400, "Bad Request"),
    };
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Parse::Bad(400, "Bad Request");
    };
    if !version.starts_with("HTTP/1.") || target.len() > MAX_TARGET_BYTES {
        return Parse::Bad(400, "Bad Request");
    }
    let mut headers = Vec::new();
    let mut content_length: usize = 0;
    let mut has_te = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = match value.parse() {
                Ok(n) => n,
                Err(_) => return Parse::Bad(400, "Bad Request"),
            };
        }
        if name == "transfer-encoding" {
            has_te = true;
        }
        headers.push((name, value));
    }
    if has_te {
        return Parse::Bad(501, "Not Implemented"); // no chunked requests
    }
    if content_length > max_body {
        return Parse::Bad(413, "Payload Too Large");
    }
    let body_start = head_end + 4;
    if rbuf.len() < body_start + content_length {
        return Parse::Partial { in_body: true };
    }
    let body = rbuf[body_start..body_start + content_length].to_vec();
    rbuf.drain(..body_start + content_length);
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (target, String::new()),
    };
    let Some(path) = url_decode(raw_path) else {
        return Parse::Bad(400, "Bad Request");
    };
    Parse::Complete(Request { method: method.into(), path, query, headers, body })
}

fn find_crlfcrlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Case-insensitive scan of a complete header block for `expect: 100-continue`.
fn expects_continue(rbuf: &[u8]) -> bool {
    let Some(head_end) = find_crlfcrlf(rbuf) else { return false };
    let Ok(head) = std::str::from_utf8(&rbuf[..head_end]) else { return false };
    head.split("\r\n").skip(1).any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("expect")
                && v.trim().eq_ignore_ascii_case("100-continue")
        })
    })
}

pub fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = hex_val(*bytes.get(i + 1)?)?;
                let lo = hex_val(*bytes.get(i + 2)?)?;
                out.push(hi * 16 + lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Minimal `k=v&k2=v2` form/query parsing (values percent-decoded).
pub fn form_get(body: &str, key: &str) -> Option<String> {
    for pair in body.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k == key {
            return url_decode(v);
        }
    }
    None
}

/// JSON string escaping for the tiny emit-only JSON this suite speaks.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
