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
// SSE subscribers are never closed for a backlog — they are STARVED. Past
// SSE_SKIP_WBUF a subscriber stops receiving new broadcasts (each skipped
// event is dropped for that subscriber, not queued); below SSE_RESUME_WBUF it
// receives again and is reported via sse_take_recovered() so the app can owe
// it a whole picture. Closing was the old rule, and it was wrong twice over:
// a burst the app itself produced (a whole-screen repaint) could jump the
// backlog from under the pacing gate straight past MAX_WBUF between two
// checks, so a healthy watcher on a real link was closed for the crime of a
// scene change — and through the platform proxy that close arrived at the
// browser as permanent silence, not an error (the SSE wedge, 2026-08-16).
pub const SSE_SKIP_WBUF: usize = 256 * 1024;
const SSE_RESUME_WBUF: usize = 64 * 1024;
const IDLE_KEEPALIVE: Duration = Duration::from_secs(60);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);
// A peer that has not drained a single byte for this long is gone, whatever
// its socket claims: three missed heartbeats' worth of moving nothing. (Not
// "non-empty this long" — a starved subscriber paying down a backlog is
// non-empty for the whole paydown and very much alive.)
const WRITE_STALL: Duration = Duration::from_secs(45);
/// How often a persistent accept() failure may print. The accept loop runs
/// every turn (milliseconds apart), so an unthrottled line would bury the log
/// it is meant to make readable — but silence is worse, so the first one
/// always prints and the count carries the rest.
const ACCEPT_ERR_LOG_EVERY: Duration = Duration::from_secs(5);

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
    // An inbound event stream (POST /hid-stream): the request never ends and
    // is never answered; the chunked body is newline-delimited JSON event
    // batches, each dispatched the moment its line is complete. No per-batch
    // request parsing, no responses — input's framing cost drops to a chunk
    // header, and the path is immune to any per-request overhead by shape.
    InStream { path: &'static str, chunk_left: usize, line: Vec<u8> },
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
    held: Option<u64>,            // long-poll: parked awaiting release() with this ticket
    stuck_since: Option<Instant>, // wbuf continuously non-empty since
    starved: bool,                // SSE: backlog over SSE_SKIP_WBUF; broadcasts skip it
    recovered: bool,              // SSE: was starved, has drained; app owes a full frame
}

pub struct Server {
    listener: TcpListener,
    conns: Vec<Conn>,
    app: &'static str,
    started: Instant,
    hold_seq: u64, // long-poll tickets (see hold/release)
    accept_errs: u64,               // accept() failures since start (see poll)
    accept_err_at: Option<Instant>, // last time one was printed; throttles the line
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
        Server { listener, conns: Vec::new(), app, started: Instant::now(), hold_seq: 0,
                 accept_errs: 0, accept_err_at: None }
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

    /// Bytes still queued for the SLOWEST subscriber of `topic`.
    ///
    /// A producer of frames has to know this or it does the wrong thing twice
    /// over. Without it, a stream that outruns the link fills `wbuf` until the
    /// client trips MAX_WBUF and gets CLOSED — a viewer far enough away is
    /// disconnected for the crime of being far away, which is exactly what
    /// happened to the display stream through the relay: locally the loopback
    /// drained instantly and nothing ever backed up, so it looked perfect.
    /// A picture stream is lossy by nature; the right answer to a watcher that
    /// cannot keep up is fewer frames, not no connection.
    /// Starved subscribers are excluded: they receive nothing until they
    /// recover, so their backlog is theirs to drain — letting it gate the
    /// scan would hand one dead-behind-a-proxy peer the power to freeze the
    /// picture for every live watcher (measured doing exactly that).
    pub fn sse_backlog(&self, topic: &str) -> usize {
        self.conns
            .iter()
            .filter(|c| !c.starved)
            .filter(|c| matches!(&c.state, ConnState::Sse { topic: t, .. } if t == topic))
            .map(|c| c.wbuf.len())
            .max()
            .unwrap_or(0)
    }

    /// Bytes queued for ANY connection, across every topic and plain response.
    ///
    /// The event loop needs this to know whether it owes the host runtime a
    /// slice. A wasip2 output-stream is not the socket: bytes handed to
    /// `write` sit in the engine's stream worker until the runtime runs it,
    /// and `check_write` reports no permit (std: `WouldBlock`) while a flush
    /// is still pending. That worker only runs when the guest yields — so an
    /// app that never sleeps can queue a stream it will never deliver, with
    /// the kernel socket sitting empty and writable the whole time (measured:
    /// 200 KiB stuck, Send-Q 0, "nothing drained for 45s", and the watcher
    /// reaped for a stall that was ours).
    pub fn pending_bytes(&self) -> usize {
        self.conns.iter().map(|c| c.wbuf.len()).sum()
    }

    /// True when any subscriber of `topic` has just drained out of starvation
    /// (clears the flag). A skipped event is dropped, never delivered late, so
    /// the app owes recovered subscribers a complete picture: a whole-frame
    /// scan for a display topic, a fresh keyframe for a video one. Console-like
    /// topics may ignore this (the gap is simply lost output).
    pub fn sse_take_recovered(&mut self, topic: &str) -> bool {
        let mut any = false;
        for conn in &mut self.conns {
            if conn.recovered
                && matches!(&conn.state, ConnState::Sse { topic: t, .. } if t == topic)
            {
                conn.recovered = false;
                any = true;
            }
        }
        any
    }

    /// One pass: accept, read, parse. Returns complete requests as
    /// (conn_key, Request); answer each with respond()/upgrade_sse() before
    /// the next poll (a key is only stable until then).
    pub fn poll(&mut self, max_body: usize) -> Vec<(usize, Request)> {
        let app = self.app;
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
                        starved: false,
                        held: None,
                        recovered: false,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break, // backlog drained
                // A PER-CONNECTION failure, not a listener failure: the pending
                // peer went away before we got to it (ECONNABORTED), or a
                // signal landed mid-call (EINTR). Neither says anything about
                // the next connection, so skip it and keep draining.
                Err(e) if e.kind() == ErrorKind::Interrupted
                       || e.kind() == ErrorKind::ConnectionAborted => continue,
                // Anything else is the LISTENER refusing to hand over work, and
                // it is usually persistent (EMFILE/ENFILE clear only when
                // something closes). This arm used to be a bare `break` with no
                // log at all, which is the worst possible shape: the app keeps
                // running, the heartbeat stays green, the guest keeps stepping
                // at full speed, and NOTHING is ever served again — with not one
                // line to say why. Retry next turn, but SAY SO.
                Err(e) => {
                    self.accept_errs += 1;
                    if self.accept_err_at.map_or(true, |t| t.elapsed() >= ACCEPT_ERR_LOG_EVERY) {
                        self.accept_err_at = Some(Instant::now());
                        eprintln!("[{app}] accept failed: {e} ({:?}); {} since start \
                                   - serving NO new connections this pass",
                                  e.kind(), self.accept_errs);
                    }
                    break;
                }
            }
        }

        // Read and parse.
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        for (i, conn) in self.conns.iter_mut().enumerate() {
            // A held (long-polled) connection stays exactly as it is: its
            // answer comes via release(), and any pipelined follow-up the
            // peer optimistically sent must not be dispatched ahead of it.
            if conn.held.is_some() {
                continue;
            }
            if matches!(conn.state, ConnState::InStream { .. }) {
                let mut closing = false;
                loop {
                    match conn.stream.read(&mut buf) {
                        Ok(0) => {
                            closing = true;
                            break;
                        }
                        Ok(n) => {
                            conn.last_activity = Instant::now();
                            conn.rbuf.extend_from_slice(&buf[..n]);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            closing = true;
                            break;
                        }
                    }
                }
                let ConnState::InStream { path, chunk_left, line } = &mut conn.state else {
                    unreachable!()
                };
                let path = *path;
                let mut lines: Vec<Vec<u8>> = Vec::new();
                // Dechunk in place: hex-size line, then that many body bytes,
                // then CRLF. Body bytes split on '\n' into event lines.
                let mut consumed = 0usize;
                {
                    let r = &conn.rbuf;
                    loop {
                        if *chunk_left == 0 {
                            let Some(pos) = r[consumed..].iter().position(|&b| b == b'\n') else { break };
                            let hdr = &r[consumed..consumed + pos];
                            let hex: String = hdr.iter().map(|&b| b as char)
                                .filter(|c| c.is_ascii_hexdigit()).collect();
                            consumed += pos + 1;
                            match usize::from_str_radix(&hex, 16) {
                                Ok(0) => { consumed = r.len(); break } // terminal chunk: peer is done
                                Ok(nn) => *chunk_left = nn,
                                Err(_) => continue, // stray CRLF between chunks
                            }
                        }
                        let take = (*chunk_left).min(r.len() - consumed);
                        if take == 0 { break }
                        for &b in &r[consumed..consumed + take] {
                            if b == b'\n' {
                                if !line.is_empty() {
                                    lines.push(std::mem::take(line));
                                }
                            } else {
                                line.push(b);
                            }
                        }
                        consumed += take;
                        *chunk_left -= take;
                    }
                }
                conn.rbuf.drain(..consumed);
                if closing {
                    conn.state = ConnState::Closing;
                }
                for l in lines {
                    out.push((i, Request {
                        method: "POST".into(),
                        path: path.into(),
                        query: String::new(),
                        headers: Vec::new(),
                        body: l,
                    }));
                }
                continue;
            }
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
                    Parse::Stream(req) => {
                        // Chunked request: this connection is the stream's for
                        // as long as it lives. If the route answers instead of
                        // upgrading, respond() must close (the unread body
                        // would desync the next parse) — cleared keep_alive
                        // makes that automatic. A curl-style peer holding the
                        // body for `Expect: 100-continue` gets its interim nod.
                        conn.keep_alive = false;
                        *since = Instant::now();
                        *reading_body = false;
                        conn.sent_continue = false;
                        if req
                            .header("expect")
                            .map(|v| v.eq_ignore_ascii_case("100-continue"))
                            .unwrap_or(false)
                        {
                            conn.wbuf.extend(b"HTTP/1.1 100 Continue\r\n\r\n");
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

    /// Long-poll support: park this request's connection. The response comes
    /// later via `release`, matched by the returned ticket — the ticket is
    /// what makes a stale key harmless: if the peer went away and the slot
    /// was reused, the ticket no longer matches and release() is a no-op.
    /// A held connection is skipped by poll(), so a pipelined follow-up
    /// cannot be dispatched out of order while the hold stands.
    pub fn hold(&mut self, key: usize) -> Option<u64> {
        self.hold_seq += 1;
        let t = self.hold_seq;
        let conn = self.conns.get_mut(key)?;
        conn.held = Some(t);
        Some(t)
    }

    /// Answer a held request, located by TICKET — never by key: the reaper
    /// compacts `conns` with retain_mut, so an index taken at hold time may
    /// point at a different connection by release time. False if the hold is
    /// gone (peer died) — the caller just drops its bookkeeping.
    pub fn release(&mut self, ticket: u64, resp: Response) -> bool {
        let Some(key) = self.conns.iter().position(|c| c.held == Some(ticket)) else {
            return false;
        };
        self.conns[key].held = None;
        self.respond(key, resp);
        true
    }

    pub fn respond(&mut self, key: usize, mut resp: Response) {
        if self.conns.get(key).is_none() {
            eprintln!("[{}] DROPPED a {} response: conn key {key} is stale ({} live)",
                      self.app, resp.status, self.conns.len());
        }
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
    /// Convert this request's connection into an inbound event stream: the
    /// remaining body bytes (chunked) feed poll() as synthesized requests on
    /// `path`, one per newline-terminated line. The request itself is never
    /// answered.
    pub fn upgrade_instream(&mut self, key: usize, path: &'static str) {
        let Some(conn) = self.conns.get_mut(key) else { return };
        conn.state = ConnState::InStream { path, chunk_left: 0, line: Vec::new() };
    }

    pub fn upgrade_sse(&mut self, key: usize, topic: &str, initial: &str) {
        let (app, live) = (self.app, self.conns.len());
        let Some(conn) = self.conns.get_mut(key) else {
            // A DROPPED RESPONSE MUST NEVER BE SILENT. `key` is an index into
            // `conns`, and the reaper compacts that Vec with retain_mut, so a
            // connection removed between poll() handing out this key and the
            // handler using it shifts every index after it. The old code
            // returned here and sent nothing at all: the client then waits for
            // response headers that never come, which reaches the caller as a
            // read timeout (EAGAIN) and reads as "the app is stalled".
            //
            // That is precisely the shape of the /video failure being chased
            // (2026-08-25): GET / answers in 106ms and /audio streams fine
            // while /video alone withholds headers. If this line ever prints,
            // that is the bug, confirmed.
            eprintln!("[{app}] DROPPED a {topic} response: conn key {key} is stale \
                       ({live} live) - the client will hang waiting for headers");
            return;
        };
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
                    // A subscriber that cannot drain what it already holds
                    // gets no more; see SSE_SKIP_WBUF. Marked, not closed —
                    // flush() reports it via sse_take_recovered when it
                    // catches up, and WRITE_STALL still reaps it if the far
                    // end has actually stopped draining altogether.
                    if conn.starved || conn.wbuf.len() > SSE_SKIP_WBUF {
                        conn.starved = true;
                        continue;
                    }
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
        let app = self.app;
        // One line per involuntary drop. Rare by design, and exactly what a
        // fleet log needs when a watcher reports a stream that went quiet.
        let dropped = |conn: &Conn, why: &str| {
            if let ConnState::Sse { topic, .. } = &conn.state {
                eprintln!("[{app}] sse drop ({topic}): {why} (wbuf={})", conn.wbuf.len());
            }
        };
        self.conns.retain_mut(|conn| {
            if let ConnState::Sse { last_beat, .. } = &mut conn.state {
                if now.duration_since(*last_beat) >= SSE_HEARTBEAT {
                    *last_beat = now;
                    // A NAMED event, not a `:hb` comment. A comment keeps the
                    // connection warm through proxies, which is what it was
                    // for, but EventSource discards comments without telling
                    // the page — so a browser cannot tell a still screen from
                    // a stream that has gone silent, and it never reconnects,
                    // because a silent stream raises no error either. That is
                    // the difference between a client that recovers from a
                    // wedged stream and one that freezes on it forever.
                    chunk_into(&mut conn.wbuf, b"event: hb\ndata: 1\n\n");
                }
            }
            // Flush.
            while !conn.wbuf.is_empty() {
                let (front, _) = conn.wbuf.as_slices();
                match conn.stream.write(front) {
                    Ok(0) => { dropped(conn, "write returned 0"); return false }
                    Ok(n) => {
                        conn.wbuf.drain(..n);
                        conn.last_activity = now;
                        // Draining at all is proof of life: the stall timer
                        // measures a peer that moves NOTHING, not one whose
                        // queue never quite empties (a starved subscriber
                        // paying down its backlog stays non-empty for as
                        // long as the backlog lasts).
                        conn.stuck_since = None;
                        busy = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => { dropped(conn, &format!("write error: {e}")); return false }
                }
            }
            if conn.starved && conn.wbuf.len() < SSE_RESUME_WBUF {
                conn.starved = false;
                conn.recovered = true; // sse_take_recovered() hands this to the app
            }
            if conn.wbuf.len() > MAX_WBUF {
                // A bounded response to a client that will not drain it is a
                // dead loss: close. An SSE subscriber is different — the
                // backlog is often OUR burst (one whole-screen repaint can
                // exceed the gap between the pacing gate and this cap), and
                // closing it here is how a healthy watcher on a real link
                // lost its stream. Starve it instead; the queue only drains
                // from here on, and WRITE_STALL covers a peer that is gone.
                match conn.state {
                    ConnState::Sse { .. } => conn.starved = true,
                    _ => return false, // slow client
                }
            }
            // Ghost detection: a live peer drains heartbeats within a tick
            // or two; a buffer that stays wedged across three heartbeat
            // intervals marks a connection whose far end has left without
            // saying so. Applies to every state — a mid-response HTTP
            // client that accepts nothing for 45s is equally gone.
            match (conn.wbuf.is_empty(), conn.stuck_since) {
                (true, _) => conn.stuck_since = None,
                (false, None) => conn.stuck_since = Some(now),
                (false, Some(t0)) if now.duration_since(t0) > WRITE_STALL => {
                    dropped(conn, "write stall: nothing drained for 45s");
                    return false;
                }
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
                // An inbound event stream is kept exactly as long as its peer
                // keeps feeding it (the bridge sends a keepalive batch well
                // inside this window); silence means the peer is gone.
                ConnState::InStream { .. } =>
                    now.duration_since(conn.last_activity) < IDLE_KEEPALIVE,
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
    /// A chunked request, delivered at end-of-headers with an empty body so
    /// the router can claim the connection via `upgrade_instream()`; the body
    /// that follows is dechunked by poll()'s InStream arm. A route that
    /// answers instead of upgrading closes the connection (poll clears
    /// keep_alive), because the unconsumed body would desync the next parse.
    Stream(Request),
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
        let body_start = head_end + 4;
        rbuf.drain(..body_start);
        let (raw_path, query) = match target.split_once('?') {
            Some((p, q)) => (p, q.to_string()),
            None => (target, String::new()),
        };
        let Some(path) = url_decode(raw_path) else {
            return Parse::Bad(400, "Bad Request");
        };
        return Parse::Stream(Request { method: method.into(), path, query, headers, body: Vec::new() });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// A Server on an ephemeral port. Built field-by-field rather than through
    /// `bind()` because `bind()` resolves its port from the environment and
    /// exits the process on failure — neither of which belongs in a test.
    fn server_on_ephemeral() -> (Server, u16) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
        listener.set_nonblocking(true).expect("non-blocking listener");
        let port = listener.local_addr().expect("local_addr").port();
        (Server {
            listener, conns: Vec::new(), app: "test", started: Instant::now(),
            hold_seq: 0, accept_errs: 0, accept_err_at: None,
        }, port)
    }

    /// Drives poll() until a request lands. The listener is non-blocking and
    /// poll() is a single pass, so a test must spin the way the real loop does.
    fn pump(s: &mut Server) -> (usize, Request) {
        for _ in 0..3000 {
            if let Some(hit) = s.poll(64 * 1024).into_iter().next() {
                return hit;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the accept loop never handed over the request");
    }

    /// The accept loop's error arms must not cost it the ordinary path. The
    /// arm this guards used to be a bare `break` that swallowed every non
    /// WouldBlock error silently; splitting it into "retry this one",
    /// "log and back off" and "backlog drained" must leave a plain request
    /// arriving, being answered, and reaching the client exactly as before.
    #[test]
    fn the_accept_loop_still_serves_an_ordinary_request() {
        let (mut s, port) = server_on_ephemeral();
        let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        c.write_all(b"GET /hi?q=1 HTTP/1.1\r\nHost: x\r\n\r\n").expect("write");

        let (key, req) = pump(&mut s);
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/hi");
        assert_eq!(req.query, "q=1");

        s.respond(key, Response::new(200, "OK").body("text/plain", "pong"));
        for _ in 0..500 {
            s.flush();
            std::thread::sleep(Duration::from_millis(1));
        }

        c.set_read_timeout(Some(Duration::from_secs(2))).expect("timeout");
        let mut buf = Vec::new();
        let _ = c.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 200"), "got: {text}");
        assert!(text.ends_with("pong"), "got: {text}");

        // A healthy accept path must never look like a failing one: the
        // counter is what a future stall gets diagnosed by.
        assert_eq!(s.accept_errs, 0, "a clean accept must not be counted as an error");
    }

    /// A backlog with several peers waiting must drain in ONE pass — the loop
    /// only stops on WouldBlock (or a real listener error), never after one.
    #[test]
    fn one_pass_drains_the_whole_backlog() {
        let (mut s, port) = server_on_ephemeral();
        let mut clients = Vec::new();
        for i in 0..5 {
            let mut c = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            write!(c, "GET /n/{i} HTTP/1.1\r\nHost: x\r\n\r\n").expect("write");
            clients.push(c);
        }
        // Give the kernel a moment to complete all five handshakes, then a
        // single poll must pick up every one of them.
        std::thread::sleep(Duration::from_millis(50));
        let got = s.poll(64 * 1024);
        assert_eq!(got.len(), 5, "one pass must drain the backlog, got {}", got.len());
        assert_eq!(s.accept_errs, 0);
    }
}
