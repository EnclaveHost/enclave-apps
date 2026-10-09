//! A streaming HTTP/1.1 server for one cooperative event loop.
//!
//! The suite's nanhttpd buffers whole requests and responses; git needs
//! neither: a push is a chunked request body of any size, a clone a
//! response of hundreds of megabytes. Here a handler first sees the request
//! head and chooses how the body arrives — refused outright, buffered (with
//! gzip decoding, which git uses for large fetch requests), or streamed
//! into a `Sink` as it is read. A response body is either bytes or a
//! `Source` pulled whenever the connection's write buffer runs low, so a
//! slow client throttles its own clone and nothing else. Sources and sinks
//! do bounded work per call; the loop rotates through every connection.
//!
//! The platform's in-enclave TLS proxy forwards plain HTTP/1.1 to the
//! loopback port named in `ENCLAVE_PORTS` (never hardcode it).

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 32 * 1024;
const MAX_CONNS: usize = 256;
const HIGH_WATER: usize = 1 << 20;
const READ_SLICE: usize = 1 << 20;
const PULL_SLICE: Duration = Duration::from_millis(25);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_IDLE: Duration = Duration::from_secs(180);
const KEEPALIVE_IDLE: Duration = Duration::from_secs(75);
const WRITE_STALL: Duration = Duration::from_secs(300);

pub struct Head {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, n: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    }
    pub fn param(&self, k: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (a, b) = kv.split_once('=').unwrap_or((kv, ""));
            (a == k).then(|| url_decode(b).unwrap_or_default())
        })
    }
}

pub trait Source<A> {
    /// Append output; Ok(true) when the body is complete. May append nothing
    /// while it works (it is called again promptly).
    fn pull(&mut self, app: &mut A, out: &mut Vec<u8>) -> Result<bool, String>;
}

pub trait Sink<A> {
    fn data(&mut self, app: &mut A, d: &[u8]) -> Result<(), String>;
    fn end(self: Box<Self>, app: &mut A) -> Response<A>;
    /// The body failed (bad framing, a sink error, the client went away).
    fn fail(self: Box<Self>, app: &mut A, err: String) -> Response<A>;
}

pub enum Body<A> {
    Full(Vec<u8>),
    Stream(Box<dyn Source<A>>),
}

pub struct Response<A> {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body<A>,
}

impl<A> Response<A> {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Body::Full(Vec::new()),
        }
    }
    pub fn with(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    pub fn bytes(mut self, ct: &str, b: impl Into<Vec<u8>>) -> Self {
        self.headers.push(("content-type".into(), ct.into()));
        self.body = Body::Full(b.into());
        self
    }
    pub fn text(status: u16, msg: &str) -> Self {
        Response::new(status).bytes("text/plain; charset=utf-8", format!("{msg}\n"))
    }
    pub fn json(status: u16, v: &serde_json::Value) -> Self {
        Response::new(status)
            .with("cache-control", "no-store")
            .bytes("application/json", v.to_string())
    }
    pub fn stream(mut self, ct: &str, s: Box<dyn Source<A>>) -> Self {
        self.headers.push(("content-type".into(), ct.into()));
        self.body = Body::Stream(s);
        self
    }
}

pub enum Plan<A> {
    /// answer without reading the body (the connection closes if one follows)
    Respond(Response<A>),
    /// read the whole body (up to this many bytes, after decoding), then `handle`
    Buffer(usize),
    Stream(Box<dyn Sink<A>>),
}

pub trait Handler: Sized {
    fn plan(&mut self, head: &Head) -> Plan<Self>;
    fn handle(&mut self, head: &Head, body: Vec<u8>) -> Response<Self>;
}

enum Chunk {
    Size(Vec<u8>),
    Data(u64),
    DataEnd(u8),
    Trailer(Vec<u8>),
    Done,
}

enum Decoder {
    Length(u64),
    Chunked(Chunk),
}

impl Decoder {
    fn done(&self) -> bool {
        matches!(self, Decoder::Length(0) | Decoder::Chunked(Chunk::Done))
    }
    /// Decode from `inp` into `out`; returns bytes consumed.
    fn decode(&mut self, inp: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
        match self {
            Decoder::Length(n) => {
                let k = (*n).min(inp.len() as u64) as usize;
                out.extend_from_slice(&inp[..k]);
                *n -= k as u64;
                Ok(k)
            }
            Decoder::Chunked(st) => {
                let mut i = 0;
                while i < inp.len() {
                    match st {
                        Chunk::Size(line) => {
                            let b = inp[i];
                            i += 1;
                            if b == b'\n' {
                                let l = String::from_utf8_lossy(line).trim().to_string();
                                let hexpart = l.split(';').next().unwrap_or("").trim();
                                let n = u64::from_str_radix(hexpart, 16)
                                    .map_err(|_| "bad chunk size")?;
                                *st = if n == 0 {
                                    Chunk::Trailer(Vec::new())
                                } else {
                                    Chunk::Data(n)
                                };
                            } else {
                                line.push(b);
                                if line.len() > 256 {
                                    return Err("chunk size line too long".into());
                                }
                            }
                        }
                        Chunk::Data(n) => {
                            let k = (*n).min((inp.len() - i) as u64) as usize;
                            out.extend_from_slice(&inp[i..i + k]);
                            i += k;
                            *n -= k as u64;
                            if *n == 0 {
                                *st = Chunk::DataEnd(0);
                            }
                        }
                        Chunk::DataEnd(seen) => {
                            let b = inp[i];
                            i += 1;
                            match (*seen, b) {
                                (0, b'\r') => *seen = 1,
                                (0, b'\n') | (1, b'\n') => *st = Chunk::Size(Vec::new()),
                                _ => return Err("bad chunk terminator".into()),
                            }
                        }
                        Chunk::Trailer(line) => {
                            let b = inp[i];
                            i += 1;
                            if b == b'\n' {
                                if line.iter().all(|&c| c == b'\r') {
                                    *st = Chunk::Done;
                                    return Ok(i);
                                }
                                line.clear();
                            } else {
                                line.push(b);
                                if line.len() > 4096 {
                                    return Err("chunk trailer too long".into());
                                }
                            }
                        }
                        Chunk::Done => return Ok(i),
                    }
                }
                Ok(i)
            }
        }
    }
}

enum Target<A> {
    Buffer { buf: Vec<u8>, max: usize },
    Sink(Box<dyn Sink<A>>),
}

enum Phase<A> {
    Head,
    Body {
        head: Head,
        dec: Decoder,
        target: Target<A>,
    },
    Out {
        src: Box<dyn Source<A>>,
    },
    Done,
    Close,
}

struct Conn<A> {
    stream: TcpStream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    phase: Phase<A>,
    keep_alive: bool,
    since: Instant,
    last_read: Instant,
    last_write: Instant,
    peer_eof: bool,
}

pub struct Server<A> {
    listener: TcpListener,
    conns: Vec<Conn<A>>,
    pub name: String,
    pub port: u16,
}

/// `ENCLAVE_PORTS=http:8000=18321,...` -> the actual port to bind.
pub fn resolve_port(default: u16) -> u16 {
    let Ok(ports) = std::env::var("ENCLAVE_PORTS") else {
        return default;
    };
    let mut first = None;
    for entry in ports.split(',') {
        let Some((label, actual)) = entry.split_once('=') else {
            continue;
        };
        let Ok(p) = actual.trim().parse::<u16>() else {
            continue;
        };
        if label.trim_start().starts_with("http:") {
            return p;
        }
        first.get_or_insert(p);
    }
    first.unwrap_or(default)
}

fn reason(s: u16) -> &'static str {
    match s {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub fn url_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn parse_head(raw: &[u8]) -> Result<Head, u16> {
    let text = std::str::from_utf8(raw).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let rl = lines.next().unwrap_or("");
    let mut parts = rl.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(400);
    };
    if !version.starts_with("HTTP/1.") || target.len() > 8192 || !target.starts_with('/') {
        return Err(400);
    }
    let mut headers = Vec::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    if version == "HTTP/1.0" {
        headers.push(("x-http10".into(), "1".into()));
    }
    let (p, q) = target.split_once('?').unwrap_or((target, ""));
    // '+' is literal in a path
    let path = url_decode(&p.replace('+', "%2B")).ok_or(400u16)?;
    Ok(Head {
        method: method.to_string(),
        path,
        query: q.to_string(),
        headers,
    })
}

/// gzip (RFC 1952) member -> bytes, bounded.
pub fn gunzip(b: &[u8], max: usize) -> Result<Vec<u8>, String> {
    if b.len() < 18 || b[0] != 0x1f || b[1] != 0x8b || b[2] != 8 {
        return Err("not gzip".into());
    }
    let flg = b[3];
    let mut p = 10;
    if flg & 4 != 0 {
        let xlen = u16::from_le_bytes([
            *b.get(p).ok_or("bad gzip")?,
            *b.get(p + 1).ok_or("bad gzip")?,
        ]) as usize;
        p += 2 + xlen;
    }
    for bit in [8u8, 16] {
        if flg & bit != 0 {
            while *b.get(p).ok_or("bad gzip")? != 0 {
                p += 1;
            }
            p += 1;
        }
    }
    if flg & 2 != 0 {
        p += 2;
    }
    if p + 8 > b.len() {
        return Err("bad gzip".into());
    }
    let out = miniz_oxide::inflate::decompress_to_vec_with_limit(&b[p..b.len() - 8], max)
        .map_err(|_| "bad gzip body")?;
    let isize = u32::from_le_bytes(b[b.len() - 4..].try_into().unwrap());
    if isize != out.len() as u32 {
        return Err("gzip length mismatch".into());
    }
    Ok(out)
}

impl<A: Handler> Server<A> {
    pub fn bind(name: &str, default_port: u16) -> Result<Server<A>, String> {
        let port = resolve_port(default_port);
        let host = std::env::var("DEPOT_BIND").unwrap_or_else(|_| "127.0.0.1".into());
        let listener = TcpListener::bind((host.as_str(), port))
            .map_err(|e| format!("bind {host}:{port}: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("nonblocking listener: {e}"))?;
        Ok(Server {
            listener,
            conns: Vec::new(),
            name: name.to_string(),
            port,
        })
    }

    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    fn write_head(&self, c: &mut Conn<A>, resp: &mut Response<A>) {
        let mut h = format!(
            "HTTP/1.1 {} {}\r\nserver: {}\r\n",
            resp.status,
            reason(resp.status),
            self.name
        );
        let streaming = matches!(resp.body, Body::Stream(_));
        if resp.status >= 500 && !streaming {
            c.keep_alive = false;
        }
        for (k, v) in &resp.headers {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        match &resp.body {
            Body::Full(b) => h.push_str(&format!("content-length: {}\r\n", b.len())),
            Body::Stream(_) => h.push_str("transfer-encoding: chunked\r\n"),
        }
        h.push_str(if c.keep_alive {
            "connection: keep-alive\r\n\r\n"
        } else {
            "connection: close\r\n\r\n"
        });
        c.wbuf.extend_from_slice(h.as_bytes());
    }

    fn start_response(&self, c: &mut Conn<A>, mut resp: Response<A>) {
        self.write_head(c, &mut resp);
        match resp.body {
            Body::Full(b) => {
                c.wbuf.extend_from_slice(&b);
                c.phase = Phase::Done;
            }
            Body::Stream(src) => c.phase = Phase::Out { src },
        }
    }

    /// One pass over every connection. Returns true when anything moved.
    pub fn step(&mut self, app: &mut A) -> bool {
        let mut busy = false;
        loop {
            match self.listener.accept() {
                Ok((s, _)) => {
                    if self.conns.len() >= MAX_CONNS || s.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let _ = s.set_nodelay(true);
                    let now = Instant::now();
                    self.conns.push(Conn {
                        stream: s,
                        rbuf: Vec::new(),
                        wbuf: Vec::new(),
                        wpos: 0,
                        phase: Phase::Head,
                        keep_alive: true,
                        since: now,
                        last_read: now,
                        last_write: now,
                        peer_eof: false,
                    });
                    busy = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        let mut conns = std::mem::take(&mut self.conns);
        for c in conns.iter_mut() {
            busy |= self.service(c, app);
        }
        let now = Instant::now();
        conns.retain(|c| {
            if matches!(c.phase, Phase::Close) && c.wpos >= c.wbuf.len() {
                return false;
            }
            match &c.phase {
                Phase::Head if c.rbuf.is_empty() => {
                    now.duration_since(c.last_read.max(c.last_write)) < KEEPALIVE_IDLE
                }
                Phase::Head => now.duration_since(c.since) < HEAD_TIMEOUT,
                Phase::Body { .. } => now.duration_since(c.last_read) < BODY_IDLE,
                _ => c.wpos >= c.wbuf.len() || now.duration_since(c.last_write) < WRITE_STALL,
            }
        });
        self.conns = conns;
        busy
    }

    fn read_some(c: &mut Conn<A>) -> bool {
        let mut moved = false;
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0;
        while total < READ_SLICE && !c.peer_eof {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.peer_eof = true;
                }
                Ok(n) => {
                    c.rbuf.extend_from_slice(&buf[..n]);
                    c.last_read = Instant::now();
                    total += n;
                    moved = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    c.peer_eof = true;
                    c.phase = Phase::Close;
                    c.wbuf.clear();
                    c.wpos = 0;
                }
            }
        }
        moved
    }

    fn flush(c: &mut Conn<A>) -> bool {
        let mut moved = false;
        while c.wpos < c.wbuf.len() {
            match c.stream.write(&c.wbuf[c.wpos..]) {
                Ok(0) => {
                    c.phase = Phase::Close;
                    c.wbuf.clear();
                    c.wpos = 0;
                    break;
                }
                Ok(n) => {
                    c.wpos += n;
                    c.last_write = Instant::now();
                    moved = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    c.phase = Phase::Close;
                    c.wbuf.clear();
                    c.wpos = 0;
                    break;
                }
            }
        }
        if c.wpos > 0 && c.wpos == c.wbuf.len() {
            c.wbuf.clear();
            c.wpos = 0;
        } else if c.wpos > HIGH_WATER {
            c.wbuf.drain(..c.wpos);
            c.wpos = 0;
        }
        moved
    }

    fn service(&self, c: &mut Conn<A>, app: &mut A) -> bool {
        let mut busy = false;
        let reading = matches!(c.phase, Phase::Head | Phase::Body { .. });
        if reading && c.rbuf.len() < READ_SLICE * 2 {
            busy |= Self::read_some(c);
        }
        loop {
            let phase = std::mem::replace(&mut c.phase, Phase::Close);
            match phase {
                Phase::Head => {
                    let Some(end) = c.rbuf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        if c.rbuf.len() > MAX_HEAD {
                            self.start_response(
                                c,
                                Response::text(431, "request headers too large"),
                            );
                            c.keep_alive = false;
                            continue;
                        }
                        if c.peer_eof {
                            c.phase = Phase::Close;
                        } else {
                            c.phase = Phase::Head;
                        }
                        break;
                    };
                    c.since = Instant::now();
                    let raw: Vec<u8> = c.rbuf.drain(..end + 4).collect();
                    let head = match parse_head(&raw[..end]) {
                        Ok(h) => h,
                        Err(s) => {
                            c.keep_alive = false;
                            self.start_response(c, Response::text(s, "bad request"));
                            continue;
                        }
                    };
                    busy = true;
                    if head
                        .header("connection")
                        .is_some_and(|v| v.eq_ignore_ascii_case("close"))
                        || head.header("x-http10").is_some()
                    {
                        c.keep_alive = false;
                    }
                    let te = head
                        .header("transfer-encoding")
                        .map(|v| v.to_ascii_lowercase());
                    let cl = head.header("content-length").map(|v| v.parse::<u64>());
                    let dec = match (&te, cl) {
                        (Some(t), _) if t.contains("chunked") => {
                            Decoder::Chunked(Chunk::Size(Vec::new()))
                        }
                        (Some(_), _) => {
                            c.keep_alive = false;
                            self.start_response(
                                c,
                                Response::text(501, "unsupported transfer-encoding"),
                            );
                            continue;
                        }
                        (None, Some(Ok(n))) => Decoder::Length(n),
                        (None, Some(Err(_))) => {
                            c.keep_alive = false;
                            self.start_response(c, Response::text(400, "bad content-length"));
                            continue;
                        }
                        (None, None) => Decoder::Length(0),
                    };
                    let has_body = !dec.done();
                    match app.plan(&head) {
                        Plan::Respond(r) => {
                            if has_body {
                                c.keep_alive = false;
                            }
                            self.start_response(c, r);
                        }
                        plan => {
                            if has_body
                                && head
                                    .header("expect")
                                    .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
                            {
                                c.wbuf.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                            }
                            let target = match plan {
                                Plan::Buffer(max) => {
                                    if let Decoder::Length(n) = dec {
                                        if n as usize > max.saturating_mul(2) {
                                            c.keep_alive = false;
                                            self.start_response(
                                                c,
                                                Response::text(413, "request body too large"),
                                            );
                                            continue;
                                        }
                                    }
                                    Target::Buffer {
                                        buf: Vec::new(),
                                        max,
                                    }
                                }
                                Plan::Stream(s) => {
                                    if head
                                        .header("content-encoding")
                                        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
                                    {
                                        c.keep_alive = false;
                                        self.start_response(
                                            c,
                                            s.fail(
                                                app,
                                                "compressed request bodies are not accepted here"
                                                    .into(),
                                            ),
                                        );
                                        continue;
                                    }
                                    Target::Sink(s)
                                }
                                Plan::Respond(_) => unreachable!(),
                            };
                            c.phase = Phase::Body { head, dec, target };
                        }
                    }
                }
                Phase::Body {
                    head,
                    mut dec,
                    mut target,
                } => {
                    let mut out = Vec::new();
                    let used = match dec.decode(&c.rbuf, &mut out) {
                        Ok(n) => n,
                        Err(e) => {
                            c.keep_alive = false;
                            let r = match target {
                                Target::Sink(s) => s.fail(app, e),
                                Target::Buffer { .. } => Response::text(400, &e),
                            };
                            self.start_response(c, r);
                            continue;
                        }
                    };
                    c.rbuf.drain(..used);
                    if !out.is_empty() {
                        busy = true;
                    }
                    match &mut target {
                        Target::Buffer { buf, max } => {
                            buf.extend_from_slice(&out);
                            if buf.len() > max.saturating_mul(2) {
                                c.keep_alive = false;
                                self.start_response(
                                    c,
                                    Response::text(413, "request body too large"),
                                );
                                continue;
                            }
                        }
                        Target::Sink(s) => {
                            if !out.is_empty() {
                                if let Err(e) = s.data(app, &out) {
                                    c.keep_alive = false;
                                    let Target::Sink(s) = target else {
                                        unreachable!()
                                    };
                                    self.start_response(c, s.fail(app, e));
                                    continue;
                                }
                            }
                        }
                    }
                    if dec.done() {
                        busy = true;
                        let r = match target {
                            Target::Sink(s) => s.end(app),
                            Target::Buffer { buf, max } => {
                                let body = if head
                                    .header("content-encoding")
                                    .is_some_and(|v| v.eq_ignore_ascii_case("gzip"))
                                {
                                    match gunzip(&buf, max) {
                                        Ok(b) => b,
                                        Err(e) => {
                                            self.start_response(c, Response::text(400, &e));
                                            continue;
                                        }
                                    }
                                } else if buf.len() > max {
                                    self.start_response(
                                        c,
                                        Response::text(413, "request body too large"),
                                    );
                                    continue;
                                } else {
                                    buf
                                };
                                app.handle(&head, body)
                            }
                        };
                        self.start_response(c, r);
                        continue;
                    }
                    if c.peer_eof {
                        c.keep_alive = false;
                        let r = match target {
                            Target::Sink(s) => {
                                s.fail(app, "the client closed the connection mid-request".into())
                            }
                            Target::Buffer { .. } => Response::text(400, "truncated request"),
                        };
                        // nobody is left to read it; drop the response
                        drop(r);
                        c.phase = Phase::Close;
                        break;
                    }
                    c.phase = Phase::Body { head, dec, target };
                    break;
                }
                Phase::Out { mut src } => {
                    let t0 = Instant::now();
                    let mut finished = false;
                    let mut failed = false;
                    while c.wbuf.len() - c.wpos < HIGH_WATER && t0.elapsed() < PULL_SLICE {
                        let mut out = Vec::new();
                        match src.pull(app, &mut out) {
                            Ok(done) => {
                                if !out.is_empty() {
                                    c.wbuf.extend_from_slice(
                                        format!("{:x}\r\n", out.len()).as_bytes(),
                                    );
                                    c.wbuf.extend_from_slice(&out);
                                    c.wbuf.extend_from_slice(b"\r\n");
                                }
                                busy = true;
                                if done {
                                    c.wbuf.extend_from_slice(b"0\r\n\r\n");
                                    finished = true;
                                    break;
                                }
                            }
                            Err(e) => {
                                eprintln!("[depot] response aborted: {e}");
                                failed = true;
                                break;
                            }
                        }
                    }
                    if failed {
                        // no terminal chunk: the client must see a broken body, not a short one
                        c.keep_alive = false;
                        c.phase = Phase::Close;
                    } else if finished {
                        c.phase = Phase::Done;
                    } else {
                        c.phase = Phase::Out { src };
                    }
                    break;
                }
                Phase::Done => {
                    if c.wpos < c.wbuf.len() {
                        c.phase = Phase::Done;
                        break;
                    }
                    if c.keep_alive && !c.peer_eof {
                        c.phase = Phase::Head;
                        c.since = Instant::now();
                        if c.rbuf.windows(4).any(|w| w == b"\r\n\r\n") {
                            continue; // a pipelined request is waiting
                        }
                        break;
                    }
                    if c.keep_alive && c.peer_eof && !c.rbuf.is_empty() {
                        c.phase = Phase::Head;
                        continue;
                    }
                    c.phase = Phase::Close;
                    break;
                }
                Phase::Close => {
                    c.phase = Phase::Close;
                    break;
                }
            }
        }
        busy |= Self::flush(c);
        if matches!(c.phase, Phase::Done) && c.wpos >= c.wbuf.len() {
            // finish the keep-alive transition promptly
            if c.keep_alive && !c.peer_eof {
                c.phase = Phase::Head;
            } else {
                c.phase = Phase::Close;
            }
        }
        busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_decoding_any_split() {
        let wire = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nx-trailer: 1\r\n\r\nNEXT";
        for split in 1..wire.len() {
            let mut d = Decoder::Chunked(Chunk::Size(Vec::new()));
            let mut out = Vec::new();
            let mut used = d.decode(&wire[..split], &mut out).unwrap();
            if !d.done() {
                used += d.decode(&wire[used..], &mut out).unwrap();
            }
            assert!(d.done(), "split {split}");
            assert_eq!(out, b"hello world");
            assert_eq!(&wire[used..], b"NEXT");
        }
        let mut d = Decoder::Chunked(Chunk::Size(Vec::new()));
        assert!(d.decode(b"zz\r\n", &mut Vec::new()).is_err());
    }

    #[test]
    fn heads_and_ports() {
        let h = parse_head(b"GET /a%20b/c+d.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: x\r\nGit-Protocol: version=2").unwrap();
        assert_eq!(h.path, "/a b/c+d.git/info/refs");
        assert_eq!(h.param("service").as_deref(), Some("git-upload-pack"));
        assert_eq!(h.header("git-protocol"), Some("version=2"));
        assert!(parse_head(b"GET x HTTP/1.1").is_err());
        std::env::set_var("ENCLAVE_PORTS", "tcp:7777=1,http:8000=18321");
        assert_eq!(resolve_port(8080), 18321);
        std::env::remove_var("ENCLAVE_PORTS");
    }

    #[test]
    fn gzip_members() {
        // python3 -c "import gzip;print(list(gzip.compress(b'hi there',mtime=0)))"
        let gz = [
            31u8, 139, 8, 0, 0, 0, 0, 0, 2, 255, 203, 200, 84, 40, 201, 72, 45, 74, 5, 0, 236, 118,
            163, 227, 8, 0, 0, 0,
        ];
        assert_eq!(gunzip(&gz, 100).unwrap(), b"hi there");
        assert!(gunzip(&gz, 3).is_err());
        assert!(gunzip(b"nope", 100).is_err());
    }
}
