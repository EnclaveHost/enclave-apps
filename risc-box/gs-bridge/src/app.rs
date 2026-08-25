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

impl Conn {
    /// The underlying TCP socket, for setting timeouts under either transport.
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => s.get_ref(),
        }
    }
    fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.tcp().set_read_timeout(d)
    }

    /// A ~1ms peek at the read side. Ok(n>0) and EOF both mean the peer said
    /// something (or hung up); a timeout means silence. Restores the normal
    /// read timeout either way.
    fn read_peek(&mut self, buf: &mut [u8]) -> PeekResult {
        let _ = self.set_read_timeout(Some(Duration::from_millis(1)));
        let r = self.read(buf);
        let _ = self.set_read_timeout(Some(Duration::from_secs(30)));
        match r {
            Ok(0) => PeekResult::Spoke, // EOF: the peer is done with us
            Ok(_) => PeekResult::Spoke,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
                PeekResult::Silent,
            Err(_) => PeekResult::Spoke,
        }
    }
}

#[derive(PartialEq)]
enum PeekResult {
    Spoke,
    Silent,
}

/// Idle connections kept for reuse. Small on purpose: the callers are the
/// input drainer and the screen, so two is the steady state and four absorbs
/// a burst without holding sockets open on the app for no reason.
const IDLE_MAX: usize = 4;

/// How long a parked connection is still trusted. The relay cuts idle spliced
/// connections well before a minute is up, and a dead pooled socket costs the
/// next input event a discovered-EOF plus a fresh TCP+TLS dial — the exact
/// burst-start hitch the pool exists to prevent. Past this age the redial is
/// cheaper than the gamble, so the connection is dropped on the floor instead
/// of handed out.
/// How long a connect (and its TLS handshake) may take before it is treated
/// as a failure worth retrying. Short on purpose — see App::connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

const IDLE_FRESH: Duration = Duration::from_secs(20);

/// How long an SSE stream may go completely silent before it is treated as
/// wedged and redialled. Above the app's 15 s SSE heartbeat, so a still screen
/// is never mistaken for a dead one.
const SSE_STALL: Duration = Duration::from_secs(20);

pub struct App {
    /// host:port of the RISC Box app, e.g. "127.0.0.1:8000".
    addr: String,
    /// Hostname without the port — TLS needs it for SNI and cert validation,
    /// and it is what belongs in the Host header.
    host: String,
    /// Path prefix prepended to every request ("" or "/x/<id>") — the direct
    /// enclave endpoint serves tenants by path, not by subdomain.
    base_path: String,
    tls: bool,
    /// Bearer token for a deployment whose config sets `api_key`.
    api_key: Option<String>,
    /// Deployment-scoped app token for a PRIVATE deployment, sent as the
    /// gateway's `enclave_app` cookie (the only owner proof that survives the
    /// relay on the data path; Authorization is consumed by the gateway).
    app_cookie: Option<String>,
    /// Warm connections for the one-shot calls (`get` / `post_json`).
    ///
    /// Dialling per request is free beside the app and ruinous across the
    /// internet, and this client was written for the former. Measured against
    /// a deployment on the fleet at 171 ms RTT: 1.62 s for a request on a new
    /// connection, 0.35 s for the same request on a warm one. `/hid` pays that
    /// on EVERY input event, so a mouse move cost about a second and a half of
    /// TCP and TLS handshaking to deliver a 60-byte body.
    ///
    /// Nothing on the app's side ever wanted this: its httpd keeps connections
    /// alive and frames every response with content-length (`src/httpd.rs`).
    /// The only thing closing them was this client sending `Connection: close`.
    ///
    /// A free list rather than one slot, because the screen's `get` must not
    /// put the input drainer's `/hid` behind it — they are different threads
    /// sharing one `Arc<App>`, and HTTP/1.1 has no way to interleave two
    /// exchanges on one socket.
    /// Each entry carries when it was parked; `take_idle` refuses anything
    /// older than [`IDLE_FRESH`].
    idle: std::sync::Mutex<Vec<(Conn, std::time::Instant)>>,
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
        let (rest, base_path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
            None => (rest, String::new()),
        };
        let host = rest.split(':').next().unwrap_or(rest).to_string();
        let addr = match rest.contains(':') {
            true => rest.to_string(),
            false => format!("{rest}:{}", if tls { 443 } else { 80 }),
        };
        App { addr, host, base_path, tls, api_key: None, app_cookie: None, idle: std::sync::Mutex::new(Vec::new()) }
    }

    /// Set the bearer token sent with every request.
    pub fn with_api_key(mut self, key: Option<String>) -> App {
        self.api_key = key;
        self
    }

    /// Set the private-deployment app token (rides as the enclave_app cookie).
    pub fn with_app_cookie(mut self, tok: Option<String>) -> App {
        self.app_cookie = tok;
        self
    }

    /// Whether the streamed input channel (POST /hid-stream, a chunked body
    /// dispatched line by line) can be TRUSTED on this path. It only works
    /// where nothing buffers the request body: a proxy that streams bodies
    /// silently — the enclave's direct endpoint does — accepts the channel,
    /// never answers, and delivers NOTHING, so every input event vanishes
    /// into its buffer while the picture keeps moving (measured: a /hid move
    /// lands the cursor, the identical /hid-stream line does not). The
    /// gateway at least 501s, which the fallback catches; a silent buffer is
    /// undetectable from this side, so the channel is loopback-only.
    pub fn input_stream_viable(&self) -> bool {
        !self.tls && self.base_path.is_empty()
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn auth_header(&self) -> String {
        // A private deployment has TWO checks with DIFFERENT keys: the gateway
        // authenticates the OWNER, and the app checks its OWN api_key. The
        // owner token rides as Bearer (the /x/ gateway keeps Authorization)
        // AND as the enclave_app cookie (the app.enclave.host gateway consumes
        // Authorization, so the cookie is its channel). The app's key then has
        // to go as x-api-key, since Authorization is spoken for. A public
        // deployment has no owner token, so the api_key takes Bearer itself.
        let mut h = String::new();
        match (&self.app_cookie, &self.api_key) {
            (Some(t), Some(k)) => h.push_str(&format!(
                "Authorization: Bearer {t}\r\nCookie: enclave_app={t}\r\nx-api-key: {k}\r\n"
            )),
            (Some(t), None) => h.push_str(&format!(
                "Authorization: Bearer {t}\r\nCookie: enclave_app={t}\r\n"
            )),
            (None, Some(k)) => h.push_str(&format!(
                "Authorization: Bearer {k}\r\nx-api-key: {k}\r\n"
            )),
            (None, None) => {}
        }
        h
    }

    fn connect(&self) -> std::io::Result<Conn> {
        // BOUND THE CONNECT. TcpStream::connect has no timeout, so against a
        // host that is refusing connections it hangs for whatever the OS
        // decides — and the audio and input paths dial at SESSION start, not
        // bridge start. One hung attempt is therefore a dead mouse and a
        // silent stream for its whole duration, while the video keeps playing
        // on the connection the bridge opened earlier. Measured on kryptos:
        // a single failed /audio connect cost 4368 silent Opus frames, 21
        // seconds, before the 1 s retry succeeded. Failing fast and retrying
        // is strictly better than waiting: the retry usually works.
        let mut s = None;
        let mut last: Option<std::io::Error> = None;
        for sa in std::net::ToSocketAddrs::to_socket_addrs(&self.addr)? {
            match TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT) {
                Ok(c) => { s = Some(c); break; }
                Err(e) => last = Some(e),
            }
        }
        let s = match s {
            Some(s) => s,
            None => return Err(last.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "no address connected")
            })),
        };
        s.set_nodelay(true)?;
        // The handshake gets the same short leash as the connect: a peer that
        // accepts the socket and then stalls mid-handshake hangs just as long
        // as one that never accepts. Restored to the generous timeout below,
        // once there is a working session to be patient with.
        s.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        s.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        if !self.tls {
            s.set_read_timeout(Some(Duration::from_secs(30)))?;
            s.set_write_timeout(Some(Duration::from_secs(30)))?;
            return Ok(Conn::Plain(s));
        }
        let handshake_sock = s.try_clone()?;
        // Generous: a frame read crosses the internet on the remote path, and
        // the app's event loop can be mid-instruction-batch when it arrives.
        let connector = SslConnector::builder(SslMethod::tls_client())
            .map_err(|e| std::io::Error::other(format!("tls setup: {e}")))?
            .build();
        let stream = connector
            .connect(&self.host, s)
            .map_err(|e| std::io::Error::other(format!("tls handshake with {}: {e}", self.host)))?;
        let _ = handshake_sock.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = handshake_sock.set_write_timeout(Some(Duration::from_secs(30)));
        Ok(Conn::Tls(Box::new(stream)))
    }

    fn take_idle(&self) -> Option<Conn> {
        let mut v = self.idle.lock().ok()?;
        // Newest first; anything stale enough to refuse means everything older
        // is stale too, so the leftovers are dropped rather than re-offered.
        while let Some((c, parked)) = v.pop() {
            if parked.elapsed() < IDLE_FRESH {
                return Some(c);
            }
            v.clear();
        }
        None
    }

    fn put_idle(&self, c: Conn) {
        if let Ok(mut v) = self.idle.lock() {
            if v.len() < IDLE_MAX {
                v.push((c, std::time::Instant::now()));
            }
        }
    }

    /// One request/response, on a warm connection when there is one.
    ///
    /// Retried once, and only when the connection came from the pool: a socket
    /// that has been idle can have been closed by the peer since we last used
    /// it, and we discover that on the write or the first read rather than up
    /// front. That failure is not the request failing, it is the connection
    /// having expired, so it earns a fresh dial. A connection we just opened
    /// failing is a real error and is returned as one.
    fn round_trip(&self, head: &str, body: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut pooled = self.take_idle();
        loop {
            let reused = pooled.is_some();
            let mut c = match pooled.take() {
                Some(c) => c,
                None => self.connect()?,
            };
            match Self::exchange(&mut c, head, body) {
                Ok((resp, keep)) => {
                    if keep {
                        self.put_idle(c);
                    }
                    return Ok(resp);
                }
                Err(e) if reused => {
                    // fall through to a fresh dial exactly once
                    let _ = e;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Write one request and read exactly one response. Returns the body and
    /// whether the connection may be kept. Reading is bounded by
    /// content-length rather than by EOF, which is what makes reuse possible
    /// at all: read-to-EOF needs the server to hang up to know it is done.
    fn exchange(c: &mut Conn, head: &str, body: &[u8]) -> std::io::Result<(Vec<u8>, bool)> {
        c.write_all(head.as_bytes())?;
        if !body.is_empty() {
            c.write_all(body)?;
        }
        c.flush()?;

        let mut raw: Vec<u8> = Vec::with_capacity(8192);
        let mut buf = [0u8; 8192];
        let hdr_end = loop {
            if let Some(p) = find(&raw, b"\r\n\r\n") {
                break p + 4;
            }
            let n = c.read(&mut buf)?;
            if n == 0 {
                return Err(std::io::Error::other("closed before the response headers"));
            }
            raw.extend_from_slice(&buf[..n]);
        };
        let head_txt = String::from_utf8_lossy(&raw[..hdr_end]).to_ascii_lowercase();
        let keep = !head_txt.contains("connection: close");

        // Only the SSE stream is chunked, and that goes through get_stream.
        // Anything else arriving chunked is unexpected: drain it and retire the
        // connection rather than leave a framing desync for the next caller.
        if head_txt.contains("transfer-encoding: chunked") {
            let mut rest = Vec::new();
            c.read_to_end(&mut rest)?;
            raw.extend_from_slice(&rest);
            return Ok((dechunk(&raw[hdr_end..]), false));
        }

        let len = head_txt
            .split("\r\n")
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok());
        let Some(len) = len else {
            // No framing at all: EOF is the only end marker, so this one cannot
            // be reused.
            let mut rest = Vec::new();
            c.read_to_end(&mut rest)?;
            raw.extend_from_slice(&rest);
            return Ok((raw[hdr_end..].to_vec(), false));
        };
        while raw.len() - hdr_end < len {
            let n = c.read(&mut buf)?;
            if n == 0 {
                return Err(std::io::Error::other("closed mid-body"));
            }
            raw.extend_from_slice(&buf[..n]);
        }
        Ok((raw[hdr_end..hdr_end + len].to_vec(), keep))
    }

    /// GET a path and return the response body.
    pub fn get(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let bp = &self.base_path;
        let req = format!(
            "GET {bp}{path} HTTP/1.1\r\nHost: {}\r\n{}Accept: */*\r\n\r\n",
            self.host,
            self.auth_header()
        );
        self.round_trip(&req, &[])
    }

    /// Open a streaming GET and hand back the connection positioned just after
    /// the response headers. Used for the long-lived /display event stream,
    /// where `get()` is useless — it reads to EOF and the stream never ends.
    pub fn get_stream(&self, path: &str) -> std::io::Result<Box<dyn std::io::BufRead>> {
        let mut s = self.connect()?;
        // A stall timeout rather than no timeout, and it has to reach the
        // socket under TLS too — clearing it only for `Conn::Plain` left every
        // https app, which is every deployment on the fleet, on the 30 s
        // timeout from `connect`.
        //
        // An idle screen sends no bands for as long as it stays idle, so bands
        // cannot be what we time out on. Heartbeats can: the app's httpd sends
        // `:hb` every 15 s on an SSE connection whatever the screen is doing
        // (`src/httpd.rs`, SSE_HEARTBEAT), so silence longer than that is the
        // connection being dead rather than the picture being still.
        //
        // Something has to notice, because a wedged stream here does not close.
        // A single unrelated request to the same deployment — a `/status`, an
        // input POST from this very process — silences an open `/display`
        // stream permanently, with the socket still ESTABLISHED and its receive
        // queue empty. Reconnecting is the only way back, and a reader with no
        // timeout waits for a byte that never comes.
        s.set_read_timeout(Some(SSE_STALL))?;
        let bp = &self.base_path;
        let req = format!(
            "GET {bp}{path} HTTP/1.1\r\nHost: {}\r\n{}Accept: text/event-stream\r\n\r\n",
            self.host,
            self.auth_header()
        );
        s.write_all(req.as_bytes())?;
        let mut r = std::io::BufReader::new(s);
        // Read the STATUS LINE and act on it. This used to be consumed and
        // ignored, which meant every non-200 -- an expired token most of all --
        // arrived as "a stream that ended after 0 frames". That reads as "the
        // app is broken" when the truth is "we were refused", and it cost hours
        // of chasing the wrong layer. Worse, the caller retried it a second
        // later, forever, hammering an app that was answering correctly.
        let mut line = String::new();
        if std::io::BufRead::read_line(&mut r, &mut line)? == 0 {
            return Err(std::io::Error::other("stream closed before the status line"));
        }
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        if !(200..300).contains(&status) {
            // Drain the headers so the socket is not left mid-message, then say
            // exactly what happened.
            loop {
                line.clear();
                match std::io::BufRead::read_line(&mut r, &mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let why = match status {
                401 | 403 => "the app token is not accepted (expired? re-mint it)",
                404 => "no such deployment on this enclave (moved, or still reloading)",
                409 => "the deployment is not running",
                502 | 503 | 504 => "the enclave could not reach the app",
                _ => "unexpected status",
            };
            return Err(std::io::Error::other(format!("HTTP {status}: {why}")));
        }
        // Consume the remaining headers.
        let mut chunked = false;
        loop {
            line.clear();
            if std::io::BufRead::read_line(&mut r, &mut line)? == 0 {
                return Err(std::io::Error::other("stream closed during headers"));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            let h = line.to_ascii_lowercase();
            if h.starts_with("transfer-encoding:") && h.contains("chunked") {
                chunked = true;
            }
        }
        Ok(match chunked {
            true => Box::new(std::io::BufReader::new(Chunked::new(r))),
            false => Box::new(r),
        })
    }

    /// POST a JSON body, ignoring the response.
    ///
    /// The response is still READ to completion, not discarded by hanging up:
    /// this is the input path, it runs on a kept connection, and a body left
    /// unread is the next caller's framing error.
    pub fn post_json(&self, path: &str, body: &str) -> std::io::Result<()> {
        let bp = &self.base_path;
        let req = format!(
            "POST {bp}{path} HTTP/1.1\r\nHost: {}\r\n{}Content-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n",
            self.host,
            self.auth_header(),
            body.len()
        );
        self.round_trip(&req, body.as_bytes())?;
        Ok(())
    }

    /// A dedicated connection for firing input POSTs without waiting on each
    /// response — see [`InputPipe`].
    pub fn input_pipe(self: &std::sync::Arc<Self>) -> InputPipe {
        InputPipe { app: self.clone(), conn: None, buf: Vec::new(), pending: 0 }
    }

    /// The streamed input channel: one POST /hid-stream whose chunked body
    /// carries every batch as a newline-terminated line — no per-batch
    /// request framing and no responses at all. An app that predates the
    /// endpoint ANSWERS the request instead of consuming it forever; the
    /// first bytes it sends back are the detection signal, and the caller
    /// falls back to the pipelined per-request channel.
    pub fn input_stream(self: &std::sync::Arc<Self>) -> InputStream {
        InputStream { app: self.clone(), conn: None }
    }
}

pub struct InputStream {
    app: std::sync::Arc<App>,
    conn: Option<Conn>,
}

impl InputStream {
    fn dial(&mut self) -> std::io::Result<()> {
        let mut c = self.app.connect()?;
        let head = format!(
            "POST /hid-stream HTTP/1.1\r\nHost: {}\r\n{}transfer-encoding: chunked\r\ncontent-type: application/json\r\n\r\n",
            self.app.host, self.app.auth_header()
        );
        c.write_all(head.as_bytes())?;
        c.flush()?;
        self.conn = Some(c);
        Ok(())
    }

    /// Ship one batch line. Errors surface so the drainer can redial or
    /// switch channels.
    pub fn send(&mut self, line: &str) -> std::io::Result<()> {
        if self.conn.is_none() {
            self.dial()?;
        }
        let c = self.conn.as_mut().unwrap();
        let chunk = format!("{:x}\r\n{}\n\r\n", line.len() + 1, line);
        match c.write_all(chunk.as_bytes()).and_then(|_| c.flush()) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    /// True when the peer wrote anything back: a modern app never answers
    /// /hid-stream, so any response (or a hangup) means an old app 404'd it
    /// and the caller should fall back to the pipelined channel.
    pub fn answered(&mut self) -> bool {
        let Some(c) = self.conn.as_mut() else { return false };
        let mut probe = [0u8; 256];
        c.read_peek(&mut probe) == PeekResult::Spoke
    }
}

/// A dedicated connection that fires input POSTs WITHOUT blocking on each
/// response.
///
/// `post_json` writes a request and then reads its whole response before
/// returning, so back-to-back input costs a full round trip apiece — measured
/// ~80 ms on the fleet — and a keystroke sits behind the previous one's
/// response even though the app already applied it. On the input path that
/// round trip is pure latency.
///
/// Here requests are PIPELINED on one connection: written one after another
/// without waiting, and the responses drained opportunistically. HTTP/1.1
/// processes requests on a connection in order and replies in the same order,
/// so keystroke order is preserved exactly — we just stop paying the return
/// trip before sending the next key. The responses are tiny and unneeded, so a
/// short-timeout read discards whatever has come back and never blocks on what
/// has not. A write error, a peer close, or too many unanswered requests drops
/// the connection and the next send redials.
pub struct InputPipe {
    app: std::sync::Arc<App>,
    conn: Option<Conn>,
    /// Bytes of a partially-received response carried between drains.
    buf: Vec<u8>,
    /// Requests written whose responses have not yet been drained.
    pending: u32,
}

impl InputPipe {
    fn reset(&mut self) {
        self.conn = None;
        self.buf.clear();
        self.pending = 0;
    }

    fn head(&self, method: &str, path: &str, body_len: usize) -> String {
        let auth = self.app.auth_header();
        let bp = &self.app.base_path;
        if body_len == 0 {
            format!(
                "{method} {bp}{path} HTTP/1.1\r\nHost: {}\r\n{auth}Accept: */*\r\n\r\n",
                self.app.host
            )
        } else {
            format!(
                "{method} {bp}{path} HTTP/1.1\r\nHost: {}\r\n{auth}Content-Type: application/json\r\n\
                 Content-Length: {body_len}\r\n\r\n",
                self.app.host
            )
        }
    }

    /// Consume one complete response out of `buf`, if a whole one is present.
    /// `/hid` and `/ping` are always content-length framed, which is what lets
    /// this skip a full HTTP parser.
    fn consume_one(&mut self) -> bool {
        let Some(pos) = find(&self.buf, b"\r\n\r\n") else { return false };
        let head = String::from_utf8_lossy(&self.buf[..pos]).to_ascii_lowercase();
        let len = head
            .split("\r\n")
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let total = pos + 4 + len;
        if self.buf.len() < total {
            return false;
        }
        self.buf.drain(..total);
        self.pending = self.pending.saturating_sub(1);
        true
    }

    /// Drain responses that have already arrived, without blocking on ones that
    /// have not. Safe to call when idle to notice a dead peer.
    pub fn poll(&mut self) {
        let mut tmp = [0u8; 4096];
        loop {
            while self.consume_one() {}
            if self.pending == 0 {
                break;
            }
            let Some(conn) = self.conn.as_mut() else { break };
            match conn.read(&mut tmp) {
                Ok(0) => {
                    self.reset();
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                // A 2 ms read timeout with nothing there is the common case, not
                // an error: it means "no more responses ready", so stop draining.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(_) => {
                    self.reset();
                    break;
                }
            }
        }
    }

    /// Fire a request and return without waiting for its response. Drains any
    /// responses already back first so the socket does not accumulate, and
    /// redials on a write error or a peer that has stopped answering.
    pub fn send(&mut self, method: &str, path: &str, body: &[u8]) {
        self.poll();
        // A peer whose answers are not coming back must not let requests pile up
        // unbounded — redial and start clean.
        if self.pending > 16 {
            self.reset();
        }
        let head = self.head(method, path, body.len());
        for _ in 0..2 {
            if self.conn.is_none() {
                match self.app.connect() {
                    Ok(c) => {
                        // The short read timeout is what makes `poll` non-blocking:
                        // a drain reads what is there and times out on the rest.
                        let _ = c.set_read_timeout(Some(Duration::from_millis(2)));
                        self.conn = Some(c);
                    }
                    Err(e) => {
                        eprintln!("[control] input pipe dial failed: {e}");
                        return;
                    }
                }
            }
            let conn = self.conn.as_mut().unwrap();
            let ok = conn.write_all(head.as_bytes()).is_ok()
                && (body.is_empty() || conn.write_all(body).is_ok())
                && conn.flush().is_ok();
            if ok {
                self.pending += 1;
                return;
            }
            // A stale socket the peer already closed: drop it and redial once.
            self.reset();
        }
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

/// Decodes HTTP/1.1 chunked transfer encoding as a stream.
///
/// This has to be a `Read` and not a "dechunk the whole body" helper, because
/// the body here never ends: `/display` stays open for the life of the session.
///
/// Skipping it appears to work, which is the dangerous part. The app writes one
/// SSE event per write, so against a local app each event lands in its own
/// chunk, and a line-oriented reader that knows nothing about chunking just
/// sees a stray hex line between events and ignores it as "not a data: line".
/// Through the gateway the framing is re-cut to the transport's own sizes, so
/// chunk boundaries fall INSIDE large events — and a band split down the middle
/// loses its closing quote, fails to parse, and is dropped without a word.
///
/// The result was a bridge that mirrored every small band and silently lost
/// every big one. Since the big one is the whole-frame band the app sends when
/// a watcher joins, Moonlight got the moving parts of the screen painted onto a
/// black field that never filled in.
struct Chunked<R: std::io::BufRead> {
    inner: R,
    /// Bytes left in the chunk currently being read.
    remaining: usize,
    done: bool,
}

impl<R: std::io::BufRead> Chunked<R> {
    fn new(inner: R) -> Self {
        Chunked { inner, remaining: 0, done: false }
    }
}

impl<R: std::io::BufRead> Read for Chunked<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            // The size line, plus the blank CRLF that terminated the previous
            // chunk. Reading until a non-empty line covers both without having
            // to track which of the two we are at.
            let mut line = String::new();
            loop {
                line.clear();
                if self.inner.read_line(&mut line)? == 0 {
                    self.done = true;
                    return Ok(0);
                }
                if !line.trim().is_empty() {
                    break;
                }
            }
            // `size[;extension]` in hex.
            let size = line.trim().split(';').next().unwrap_or("").trim().to_string();
            let n = usize::from_str_radix(&size, 16).map_err(|_| {
                std::io::Error::other(format!("bad chunk size {size:?}"))
            })?;
            if n == 0 {
                self.done = true;
                return Ok(0);
            }
            self.remaining = n;
        }
        let want = out.len().min(self.remaining);
        let n = self.inner.read(&mut out[..want])?;
        if n == 0 {
            self.done = true;
            return Ok(0);
        }
        self.remaining -= n;
        Ok(n)
    }
}
