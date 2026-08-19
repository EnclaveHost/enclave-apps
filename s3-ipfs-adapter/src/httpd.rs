//! nanhttpd — the suite's zero-dependency HTTP/1.1 engine for Enclave
//! service apps, in the s3-ipfs-adapter variant: SSE is dropped (nothing
//! here streams events) and STREAMING RESPONSE BODIES are added, because a
//! gateway that serves multi-gigabyte files cannot buffer them, and because
//! the engine's slow-client rule (close past MAX_WBUF) would otherwise kill
//! any response larger than the write buffer on the first flush.
//!
//! The platform launches run-mode wasm components with `wasmtime run` and a
//! wasi:sockets grant; the deployment's `http:` port is served at its origin
//! (https://<first-8-hex-of-id>.app.enclave.host) by the enclave's in-TEE TLS
//! proxy, which forwards plain HTTP/1.1 to the loopback port we bind. That
//! gives a service app what a `wasi:http` component never gets: one live
//! process for the whole deployment, so state can live in memory.
//!
//! The one platform rule (see network-test): **read `ENCLAVE_PORTS` and bind
//! the actual port, never hardcode.** Entries look like `http:8000=18321`;
//! we prefer the first `http:` entry, fall back to the first entry, and only
//! default to 8000 when the variable is absent (local development).
//!
//! wasm32-wasip2 has no threads, so this is one non-blocking event loop:
//! accept, read/parse/dispatch, flush, reap, then a short sleep.
//!
//! Streaming shape: the app answers a request either with `respond()` (small
//! bodies, buffered) or `respond_stream()` (a `Body` source the engine PULLS
//! as the client drains). `pump()` runs at most ONE pull per call, round-robin
//! across streaming connections, so a source that does bounded blocking work
//! per pull (one S3 range request) never stalls the loop for more than one
//! request's worth; back-pressure is the wbuf low-water mark, so memory per
//! streaming client stays bounded at roughly one pull window. A client that
//! drains nothing for WRITE_STALL is reaped, which is what bounds a stream
//! wedged behind a dead proxy hop (every server-side wait on a response path
//! must keep bytes ticking; the platform gateway cuts silent streams).

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_TARGET_BYTES: usize = 4 * 1024;
pub const MAX_CONNS: usize = 384;
pub const MAX_WBUF: usize = 512 * 1024; // close a buffered client that can't drain this
const IDLE_KEEPALIVE: Duration = Duration::from_secs(60);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(120); // big-route uploads included
// A peer that has not drained a single byte for this long is gone, whatever
// its socket claims. (Not "non-empty this long": a streaming client paying
// down a window is non-empty for the whole paydown and very much alive.)
const WRITE_STALL: Duration = Duration::from_secs(45);

/// Streaming back-pressure: pump a stream only when its wbuf is below this.
const STREAM_LOW: usize = 128 * 1024;
/// At most this many concurrent streaming responses; the app 503s past it.
pub const STREAM_MAX: usize = 24;

/// A pull-based response body. Each pull may do bounded blocking work (at
/// most one upstream request) and returns Ok(None) at end of body. An Err
/// aborts the connection, which a client sees as truncation (the HTTP/1.1
/// signal for a mid-body failure).
pub trait Body {
    fn pull(&mut self) -> Result<Option<Vec<u8>>, String>;
}

/// A push-based request-body consumer for STREAMING uploads (the /add-wasm
/// route): the engine feeds body bytes as they arrive instead of buffering
/// the request, because a 2 GiB component cannot live in a wasm32 guest's
/// memory. Each feed may do bounded blocking work (at most one or two part
/// uploads); an Err(response) rejects the upload mid-body — the engine sends
/// it and closes (the sink must have cleaned up its own upstream state
/// before returning Err). `abort` is the cleanup hook for a connection that
/// dies mid-body; it too must bound its blocking work.
pub trait Sink {
    fn feed(&mut self, data: &[u8]) -> Result<(), Response>;
    fn finish(&mut self) -> Response;
    fn abort(&mut self);
}

/// Inactivity ceiling for a streaming upload: a client that sends nothing
/// for this long is gone. (Deliberately not a total-body deadline — a slow
/// but live 2 GiB upload must keep its connection.)
const RECV_STALL: Duration = Duration::from_secs(90);
/// Minimum sustained upload throughput once past the grace window. A trickle
/// slower than this is a slowloris holding an upload slot, not a slow client
/// (this floor is far below any real uploader, so it never reaps a genuine
/// upload). Bounds how long the four slots can be pinned by dead weight.
const RECV_GRACE: Duration = Duration::from_secs(120);
const MIN_RECV_RATE: u64 = 8 * 1024; // bytes/sec
/// Cap on body bytes handed to a Sink per poll (see the RecvBody feed site).
const MAX_FEED_PER_POLL: usize = 16 * 1024 * 1024;

pub struct Request {
    pub method: String,
    pub path: String,   // percent-decoded, no query
    pub query: String,  // raw, after '?'
    pub headers: Vec<(String, String)>, // names lowercased
    pub body: Vec<u8>,
    /// Headers-only request on the streaming route: the body (`stream_len`
    /// bytes) has NOT been read. The app must either call `begin_body(key,
    /// sink)` to consume it or respond an error (the engine then closes the
    /// connection rather than resynchronize past an unread body).
    pub stream_len: Option<u64>,
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
    Streaming { src: Box<dyn Body>, chunked: bool },
    RecvBody { sink: Box<dyn Sink>, remaining: u64, body_len: u64, started: Instant },
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
    stuck_since: Option<Instant>, // wbuf continuously undrained since
    // A streaming-route request was emitted and awaits begin_body(): the
    // unread body length, plus whether the client asked for 100-continue.
    pending_stream: Option<u64>,
    pending_expect: bool,
}

pub struct Server {
    listener: TcpListener,
    conns: Vec<Conn>,
    app: &'static str,
    started: Instant,
    pump_cursor: usize,
}

/// `ENCLAVE_PORTS=http:8000=18321,tcp:7777=18322` → the actual port to bind.
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
        Server { listener, conns: Vec::new(), app, started: Instant::now(), pump_cursor: 0 }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    pub fn streams_active(&self) -> usize {
        self.conns
            .iter()
            .filter(|c| matches!(c.state, ConnState::Streaming { .. }))
            .count()
    }

    /// One pass: accept, read, parse. Returns complete requests as
    /// (conn_key, Request); answer each with respond()/respond_stream()
    /// before the next poll (a key is only stable until then).
    ///
    /// `max_body` caps ordinary request bodies; a target matching a prefix in
    /// `big_routes` gets THAT route's cap (each buffered upload route has its
    /// own ceiling — a blanket cap over-buffers the smaller routes). Targets
    /// under `stream_prefix` are not buffered at all: the request is emitted
    /// at end-of-headers with `stream_len` set, and the body is consumed by
    /// the Sink the app registers via `begin_body`.
    pub fn poll(
        &mut self,
        max_body: usize,
        big_routes: &[(&str, usize)],
        stream_prefix: &str,
    ) -> Vec<(usize, Request)> {
        let biggest = big_routes.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let read_cap = MAX_HEADER_BYTES + max_body.max(biggest);
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
                        stuck_since: None,
                        pending_stream: None,
                        pending_expect: false,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Read and parse.
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        let app = self.app;
        for (i, conn) in self.conns.iter_mut().enumerate() {
            if !matches!(conn.state, ConnState::Http { .. } | ConnState::RecvBody { .. }) {
                // Streaming and closing conns: drain+discard any input.
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
            let mut peer_gone = false;
            loop {
                match conn.stream.read(&mut buf) {
                    Ok(0) => {
                        conn.keep_alive = false;
                        peer_gone = true;
                        if conn.rbuf.is_empty() && matches!(conn.state, ConnState::Http { .. }) {
                            conn.state = ConnState::Closing;
                        }
                        break;
                    }
                    Ok(n) => {
                        conn.rbuf.extend_from_slice(&buf[..n]);
                        conn.last_activity = Instant::now();
                        if conn.rbuf.len() > read_cap
                            && matches!(conn.state, ConnState::Http { .. })
                            && conn.pending_stream.is_none()
                        {
                            overflow(conn, 413, "Payload Too Large");
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        conn.keep_alive = false;
                        peer_gone = true;
                        if matches!(conn.state, ConnState::Http { .. }) {
                            conn.state = ConnState::Closing;
                            conn.wbuf.clear();
                        }
                        break;
                    }
                }
            }
            // A registered sink consumes body bytes as they arrive. Bounded
            // per poll (MAX_FEED_PER_POLL): one read can accumulate a large
            // rbuf, and feeding it whole would flush many S3 parts back to
            // back, freezing every other connection (including in-flight
            // gateway downloads) for the whole run. Capping the feed spreads
            // the blocking S3 work across polls; the rest of rbuf waits for
            // the next one.
            if let ConnState::RecvBody { sink, remaining, .. } = &mut conn.state {
                if !conn.rbuf.is_empty() && *remaining > 0 {
                    let take = (*remaining)
                        .min(conn.rbuf.len() as u64)
                        .min(MAX_FEED_PER_POLL as u64) as usize;
                    let fed = sink.feed(&conn.rbuf[..take]);
                    conn.rbuf.drain(..take);
                    *remaining -= take as u64;
                    if take > 0 {
                        conn.last_activity = Instant::now();
                    }
                    if let Err(resp) = fed {
                        conn.keep_alive = false;
                        write_response(conn, app, resp, false);
                        conn.state = ConnState::Closing;
                        continue;
                    }
                }
                if let ConnState::RecvBody { sink, remaining, .. } = &mut conn.state {
                    if *remaining == 0 {
                        let resp = sink.finish();
                        let keep = conn.keep_alive && resp.status < 500;
                        write_response(conn, app, resp, keep);
                        conn.state = if keep {
                            ConnState::Http { since: Instant::now(), reading_body: false }
                        } else {
                            ConnState::Closing
                        };
                    } else if peer_gone {
                        sink.abort();
                        conn.wbuf.clear();
                        conn.state = ConnState::Closing;
                    }
                }
                continue;
            }
            if conn.pending_stream.is_some() {
                continue; // emitted last poll; the app answers before this one
            }
            if let ConnState::Http { since, reading_body } = &mut conn.state {
                match try_parse(&mut conn.rbuf, max_body, big_routes, stream_prefix) {
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
                        if let Some(len) = req.stream_len {
                            conn.pending_stream = Some(len);
                            conn.pending_expect =
                                req.header("expect").is_some_and(|v| {
                                    v.eq_ignore_ascii_case("100-continue")
                                });
                        }
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

    /// Register the Sink that consumes a streaming request's body (a request
    /// that arrived with `stream_len` set). Any body bytes already buffered
    /// are fed on the next poll.
    pub fn begin_body(&mut self, key: usize, sink: Box<dyn Sink>) {
        let Some(conn) = self.conns.get_mut(key) else { return };
        let Some(remaining) = conn.pending_stream.take() else { return };
        if conn.pending_expect && !conn.sent_continue {
            conn.sent_continue = true;
            conn.wbuf.extend(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        conn.pending_expect = false;
        conn.state = ConnState::RecvBody {
            sink,
            remaining,
            body_len: remaining,
            started: Instant::now(),
        };
    }

    pub fn respond(&mut self, key: usize, resp: Response) {
        let Some(conn) = self.conns.get_mut(key) else { return };
        // Answering a streaming-route request WITHOUT consuming its body:
        // the engine will not resynchronize past an unread body, so the
        // connection closes after this response.
        if conn.pending_stream.take().is_some() {
            conn.keep_alive = false;
            conn.pending_expect = false;
        }
        let keep = conn.keep_alive && resp.status < 500;
        write_response(conn, self.app, resp, keep);
        if !keep {
            conn.state = ConnState::Closing;
        }
    }

    /// Answer with a streamed body pulled from `src` as the client drains.
    /// `resp.body` is ignored; `len` (when known) becomes content-length,
    /// otherwise the body goes out `Transfer-Encoding: chunked`. `head_only`
    /// (HEAD requests) sends the identical header block and no body.
    pub fn respond_stream(
        &mut self,
        key: usize,
        mut resp: Response,
        len: Option<u64>,
        head_only: bool,
        src: Box<dyn Body>,
    ) {
        let Some(conn) = self.conns.get_mut(key) else { return };
        let chunked = len.is_none();
        let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
        resp.headers.push(("server".into(), self.app.into()));
        match len {
            Some(n) => resp.headers.push(("content-length".into(), n.to_string())),
            None => resp.headers.push(("transfer-encoding".into(), "chunked".into())),
        }
        resp.headers.push((
            "connection".into(),
            if conn.keep_alive { "keep-alive" } else { "close" }.into(),
        ));
        for (k, v) in &resp.headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        conn.wbuf.extend(head.as_bytes());
        if head_only {
            if !conn.keep_alive {
                conn.state = ConnState::Closing;
            }
            return;
        }
        conn.state = ConnState::Streaming { src, chunked };
    }

    /// Advance at most ONE streaming response by one pull, round-robin, and
    /// only one whose write buffer has drained below the low-water mark.
    /// Returns whether a pull happened (the loop's "busy" signal).
    pub fn pump(&mut self) -> bool {
        let n = self.conns.len();
        if n == 0 {
            return false;
        }
        for step in 0..n {
            let i = (self.pump_cursor + step) % n;
            let conn = &mut self.conns[i];
            let ConnState::Streaming { src, chunked } = &mut conn.state else { continue };
            if conn.wbuf.len() >= STREAM_LOW {
                continue;
            }
            let chunked = *chunked;
            match src.pull() {
                Ok(Some(data)) => {
                    if !data.is_empty() {
                        if chunked {
                            chunk_into(&mut conn.wbuf, &data);
                        } else {
                            conn.wbuf.extend(&data);
                        }
                    }
                }
                Ok(None) => {
                    if chunked {
                        conn.wbuf.extend(b"0\r\n\r\n");
                    }
                    conn.state = if conn.keep_alive {
                        ConnState::Http { since: Instant::now(), reading_body: false }
                    } else {
                        ConnState::Closing
                    };
                }
                Err(e) => {
                    // Mid-body failure: keep what is queued (it is verified
                    // data), then close without the terminator so the client
                    // sees truncation, the HTTP/1.1 abort signal.
                    eprintln!("[{}] stream aborted: {e}", self.app);
                    conn.keep_alive = false;
                    conn.state = ConnState::Closing;
                }
            }
            self.pump_cursor = (i + 1) % n;
            return true;
        }
        false
    }

    /// Flush write buffers, reap dead/expired conns, sleep.
    pub fn flush_and_sleep(&mut self) {
        let busy = self.flush();
        std::thread::sleep(Duration::from_millis(if busy { 2 } else { 25 }));
    }

    /// Like `flush_and_sleep` but without the sleep, for loops with real
    /// work between polls; returns whether any bytes moved.
    pub fn flush(&mut self) -> bool {
        let now = Instant::now();
        let mut busy = false;
        self.conns.retain_mut(|conn| {
            let keep = flush_conn(conn, now, &mut busy);
            if !keep {
                // A reaped mid-body upload must release its upstream state.
                if let ConnState::RecvBody { sink, .. } = &mut conn.state {
                    sink.abort();
                }
            }
            keep
        });
        busy
    }
}

/// Flush one connection's write buffer and decide whether it lives on.
fn flush_conn(conn: &mut Conn, now: Instant, busy: &mut bool) -> bool {
    while !conn.wbuf.is_empty() {
        let (front, _) = conn.wbuf.as_slices();
        match conn.stream.write(front) {
            Ok(0) => return false,
            Ok(n) => {
                conn.wbuf.drain(..n);
                conn.last_activity = now;
                // Draining at all is proof of life: the stall timer
                // measures a peer that moves NOTHING, not one whose
                // queue never quite empties.
                conn.stuck_since = None;
                *busy = true;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => return false,
        }
    }
    // The slow-client rule is for buffered responses; a streaming
    // conn's wbuf is bounded by the pump's low-water mark + one pull
    // window, and WRITE_STALL below reaps the truly dead.
    if conn.wbuf.len() > MAX_WBUF && !matches!(conn.state, ConnState::Streaming { .. }) {
        return false;
    }
    match (conn.wbuf.is_empty(), conn.stuck_since) {
        (true, _) => conn.stuck_since = None,
        (false, None) => conn.stuck_since = Some(now),
        (false, Some(t0)) if now.duration_since(t0) > WRITE_STALL => return false,
        _ => {}
    }
    match &conn.state {
        ConnState::Closing => !conn.wbuf.is_empty(),
        ConnState::Streaming { .. } => true,
        // A streaming upload is timed on inactivity (a silent peer), plus a
        // throughput floor past a grace window (a trickle that stays under
        // RECV_STALL forever but moves almost nothing, pinning an upload
        // slot). A genuine slow 2 GiB body clears both.
        ConnState::RecvBody { remaining, body_len, started, .. } => {
            if now.duration_since(conn.last_activity) >= RECV_STALL {
                return false;
            }
            let elapsed = now.duration_since(*started);
            if elapsed > RECV_GRACE {
                let fed = body_len.saturating_sub(*remaining);
                if fed < MIN_RECV_RATE.saturating_mul(elapsed.as_secs()) {
                    return false; // below the throughput floor
                }
            }
            true
        }
        ConnState::Http { since, reading_body } => {
            let idle = now.duration_since(conn.last_activity);
            if conn.rbuf.is_empty() && !reading_body && conn.pending_stream.is_none() {
                idle < IDLE_KEEPALIVE
            } else {
                now.duration_since(*since)
                    < if *reading_body { BODY_TIMEOUT } else { HEADER_TIMEOUT }
            }
        }
    }
}

/// Serialize a buffered response onto a connection's write queue.
fn write_response(conn: &mut Conn, app: &str, mut resp: Response, keep: bool) {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
    resp.headers.push(("server".into(), app.into()));
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

fn try_parse(
    rbuf: &mut Vec<u8>,
    max_body: usize,
    big_routes: &[(&str, usize)],
    stream_prefix: &str,
) -> Parse {
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
    let mut cl_seen = 0u32;
    let mut has_te = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            cl_seen += 1;
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
    // Conflicting/duplicate Content-Length is a request-smuggling desync
    // primitive (RFC 9112 §6.3): if a hop in front resolves the length
    // differently than we do, the two disagree on where this body ends and
    // the next request begins. Refuse rather than pick a winner.
    if cl_seen > 1 {
        return Parse::Bad(400, "Bad Request");
    }
    let bare_target = target.split('?').next().unwrap_or("");
    // A streaming route's body is not buffered: emit the request at
    // end-of-headers and let the app's Sink consume the body. Its size cap
    // is the app's business (it knows the route's ceiling), not the parser's.
    let is_stream = !stream_prefix.is_empty()
        && bare_target.starts_with(stream_prefix)
        && method == "POST"
        && content_length > 0;
    // Per-route cap, enforced against Content-Length BEFORE a byte is
    // buffered (as the reference gateway's Caddy did). A single blanket
    // "big body" cap let an unauthenticated client force the largest cap's
    // worth of buffering on the smallest-cap route (e.g. 32 MiB on /add-json,
    // whose real ceiling is 1 MiB) - MAX_CONNS of those OOMs a wasm32 guest.
    let cap = big_routes
        .iter()
        .find(|(p, _)| bare_target.starts_with(p))
        .map(|(_, c)| *c)
        .unwrap_or(max_body);
    if !is_stream && content_length > cap {
        return Parse::Bad(413, "Payload Too Large");
    }
    let body_start = head_end + 4;
    if !is_stream && rbuf.len() < body_start + content_length {
        return Parse::Partial { in_body: true };
    }
    let (body, stream_len) = if is_stream {
        rbuf.drain(..body_start);
        (Vec::new(), Some(content_length as u64))
    } else {
        let body = rbuf[body_start..body_start + content_length].to_vec();
        rbuf.drain(..body_start + content_length);
        (body, None)
    };
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (target, String::new()),
    };
    // Percent-decoding only: '+' is a literal in a path (the space alias is
    // a form/query convention, and gateway paths contain real plus signs).
    let Some(path) = percent_decode(raw_path) else {
        return Parse::Bad(400, "Bad Request");
    };
    Parse::Complete(Request { method: method.into(), path, query, headers, body, stream_len })
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

/// Percent-decode without the '+'-to-space form convention (for paths).
pub fn percent_decode(s: &str) -> Option<String> {
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
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTES: [(&str, usize); 3] =
        [("/api/upload", 32 * 1024 * 1024), ("/add-json", 1024 * 1024), ("/add-image", 4 * 1024 * 1024)];

    fn parse(head: &str) -> Parse {
        let mut rbuf = head.as_bytes().to_vec();
        try_parse(&mut rbuf, 16 * 1024, &ROUTES, "/add-wasm")
    }

    #[test]
    fn per_route_cap_rejects_before_buffering() {
        // 2 MB to /add-json (1 MiB cap) is refused from the Content-Length
        // alone - the HIGH regression: a blanket 32 MiB cap would have let
        // this buffer. Same size to /api/upload (32 MiB cap) is accepted.
        assert!(matches!(
            parse("POST /add-json HTTP/1.1\r\ncontent-length: 2000000\r\n\r\n"),
            Parse::Bad(413, _)
        ));
        assert!(matches!(
            parse("POST /add-image HTTP/1.1\r\ncontent-length: 5000000\r\n\r\n"),
            Parse::Bad(413, _)
        ));
        assert!(matches!(
            parse("POST /api/upload?path=x HTTP/1.1\r\ncontent-length: 2000000\r\n\r\n"),
            Parse::Partial { in_body: true }
        ));
        // an unlisted route falls back to max_body (16 KiB)
        assert!(matches!(
            parse("POST /whatever HTTP/1.1\r\ncontent-length: 200000\r\n\r\n"),
            Parse::Bad(413, _)
        ));
    }

    #[test]
    fn duplicate_content_length_refused() {
        assert!(matches!(
            parse("POST /add-json HTTP/1.1\r\ncontent-length: 10\r\ncontent-length: 20\r\n\r\n"),
            Parse::Bad(400, _)
        ));
    }

    #[test]
    fn streaming_route_emits_headers_only() {
        match parse("POST /add-wasm HTTP/1.1\r\ncontent-length: 100000000\r\n\r\n") {
            Parse::Complete(req) => {
                assert_eq!(req.stream_len, Some(100_000_000));
                assert!(req.body.is_empty()); // body NOT buffered
            }
            other => panic!("expected Complete, got {}", parse_kind(&other)),
        }
    }

    #[test]
    fn transfer_encoding_still_refused() {
        assert!(matches!(
            parse("POST /add-wasm HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"),
            Parse::Bad(501, _)
        ));
    }

    fn parse_kind(p: &Parse) -> &'static str {
        match p {
            Parse::Complete(_) => "Complete",
            Parse::Partial { .. } => "Partial",
            Parse::Bad(..) => "Bad",
        }
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
