//! The GameStream host, driven by polling instead of threads.
//!
//! The native bridge gave every port its own thread and a blocking
//! `listener.incoming()` loop. Inside the sandbox `std::thread::spawn` is os
//! error 58, and the only loop there is is the one stepping the emulator — so
//! every socket here is non-blocking and [`Host::poll`] is called once per
//! turn, exactly like the app's own httpd. Nothing in this file may block: a
//! blocking read here stalls the emulated machine.
//!
//! Six sockets, matching what Moonlight expects:
//!   TCP 47989 http   — discovery and pairing
//!   TCP 47984 https  — applist/launch/resume, under the paired certificate
//!   TCP 48010 rtsp   — stream setup
//!   UDP 47998 video  — RTP, fed by the NVENC encoder
//!   UDP 47999 control— ENet, input and IDR requests
//!   UDP 48000 audio  — RTP

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gamestream::httpx;
use crate::gamestream::pair::Outcome;
use crate::gamestream::session::{
    PORT_AUDIO, PORT_CONTROL, PORT_HTTP, PORT_HTTPS, PORT_RTSP, PORT_VIDEO,
};
use crate::gamestream::video::AuSink;
use crate::video::EncodedFrame;

/// A request that arrives without its headers finishing is dropped at this
/// age; a parked pairing request is held rather than answered, and gets the
/// PIN window instead.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a phase-1 pairing request waits for the operator's PIN. Matches
/// the window the native bridge blocked a thread for.
const PIN_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on a single request; GameStream requests are all small.
const MAX_REQUEST: usize = 16 * 1024;

enum Kind {
    Http,
    Https(Box<rustls::ServerConnection>),
    Rtsp,
}

struct Conn {
    stream: TcpStream,
    kind: Kind,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    since: Instant,
    /// Set when a phase-1 pairing request is parked waiting for the PIN. The
    /// request is re-run on later polls rather than held on a blocked thread.
    parked: Option<String>,
    closing: bool,
}

pub struct Host {
    http: TcpListener,
    https: TcpListener,
    rtsp: TcpListener,
    video: UdpSocket,
    /// ENet owns udp/47999 itself -- it binds through the platform layer in
    /// enet_sys, so the host must NOT also bind that port.
    control: Option<crate::gamestream::enet::Host>,
    audio: UdpSocket,
    srv: Arc<httpx::Server>,
    tls: Option<Arc<rustls::ServerConfig>>,
    conns: Vec<Conn>,
    sink: Option<AuSink>,
    local_ip: String,
}

fn bind_tcp(port: u16) -> Option<TcpListener> {
    let l = TcpListener::bind(("0.0.0.0", port))
        .map_err(|e| eprintln!("[gs] cannot bind tcp/{port}: {e}"))
        .ok()?;
    l.set_nonblocking(true).ok()?;
    Some(l)
}

fn bind_udp(port: u16) -> Option<UdpSocket> {
    let s = UdpSocket::bind(("0.0.0.0", port))
        .map_err(|e| eprintln!("[gs] cannot bind udp/{port}: {e}"))
        .ok()?;
    s.set_nonblocking(true).ok()?;
    Some(s)
}

impl Host {
    /// Bind every port. `None` if any is unavailable — the app still runs, it
    /// just does not offer GameStream, which is better than half a host that
    /// Moonlight can discover but not stream from.
    pub fn bind(srv: Arc<httpx::Server>, local_ip: String) -> Option<Host> {
        let tls = match build_tls(&srv) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                // Discovery and pairing still work over plain HTTP; launch does
                // not. Say so rather than failing silently.
                eprintln!("[gs] TLS unavailable ({e}); https surface disabled");
                None
            }
        };
        let host = Host {
            http: bind_tcp(PORT_HTTP)?,
            https: bind_tcp(PORT_HTTPS)?,
            rtsp: bind_tcp(PORT_RTSP)?,
            video: bind_udp(PORT_VIDEO)?,
            control: crate::gamestream::enet::Host::bind(PORT_CONTROL),
            audio: bind_udp(PORT_AUDIO)?,
            srv,
            tls,
            conns: Vec::new(),
            sink: None,
            local_ip,
        };
        eprintln!(
            "[gs] GameStream host up: tcp {PORT_HTTP}/{PORT_HTTPS}/{PORT_RTSP}, \
             udp {PORT_VIDEO}/{PORT_CONTROL}/{PORT_AUDIO}"
        );
        Some(host)
    }

    /// One pass. Returns true if anything moved, so the caller can keep its
    /// turn budget honest.
    pub fn poll(&mut self) -> bool {
        let mut busy = false;
        busy |= self.accept();
        busy |= self.service();
        busy |= self.drain_udp();
        busy |= self.drain_control();
        self.reap();
        busy
    }

    fn accept(&mut self) -> bool {
        let mut busy = false;
        for (listener, mk) in [
            (&self.http, 0u8),
            (&self.https, 1u8),
            (&self.rtsp, 2u8),
        ] {
            loop {
                match listener.accept() {
                    Ok((s, _)) => {
                        if s.set_nonblocking(true).is_err() || self.conns.len() >= 64 {
                            continue;
                        }
                        let kind = match mk {
                            0 => Kind::Http,
                            2 => Kind::Rtsp,
                            _ => match self.tls.as_ref().and_then(|c| {
                                rustls::ServerConnection::new(c.clone()).ok()
                            }) {
                                Some(c) => Kind::Https(Box::new(c)),
                                None => continue, // no TLS: drop rather than serve plaintext
                            },
                        };
                        self.conns.push(Conn {
                            stream: s,
                            kind,
                            rbuf: Vec::new(),
                            wbuf: Vec::new(),
                            since: Instant::now(),
                            parked: None,
                            closing: false,
                        });
                        busy = true;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        busy
    }

    fn service(&mut self) -> bool {
        let mut busy = false;
        for i in 0..self.conns.len() {
            // Split the borrow: routing needs &self.srv while the connection is
            // mutable.
            let (srv, local_ip, tls_on) =
                (self.srv.clone(), self.local_ip.clone(), self.tls.is_some());
            let c = &mut self.conns[i];
            if c.closing {
                continue;
            }
            busy |= pump(c, &srv, &local_ip, tls_on);
        }
        busy
    }

    fn reap(&mut self) {
        let now = Instant::now();
        self.conns.retain(|c| {
            if c.closing && c.wbuf.is_empty() {
                return false;
            }
            let age = now.duration_since(c.since);
            match &c.parked {
                // A parked pairing request gets the PIN window, not the header
                // window: the operator is being asked for a number.
                Some(_) => age < PIN_TIMEOUT,
                None => age < HEADER_TIMEOUT || !c.wbuf.is_empty(),
            }
        });
    }

    /// Client pings establish where to send RTP; until one arrives there is no
    /// peer and the sink drops frames on the floor.
    fn drain_udp(&mut self) -> bool {
        let mut busy = false;
        let mut buf = [0u8; 2048];
        let session = self.srv.session.lock().unwrap().clone();

        while let Ok((n, peer)) = self.video.recv_from(&mut buf) {
            busy = true;
            if let Some(s) = &session {
                let mut slot = s.video_peer.lock().unwrap();
                if slot.is_none() {
                    eprintln!("[video] client ping from {peer}");
                }
                *slot = Some(peer);
            }
            let _ = n;
        }
        while let Ok((n, peer)) = self.audio.recv_from(&mut buf) {
            busy = true;
            if let Some(s) = &session {
                let mut slot = s.audio_peer.lock().unwrap();
                if slot.is_none() {
                    eprintln!("[audio] client ping from {peer}");
                }
                *slot = Some(peer);
            }
            let _ = n;
        }
        busy
    }

    /// Pump the ENet control channel. Zero timeout inside: this is the turn
    /// that steps the CPU.
    fn drain_control(&mut self) -> bool {
        use crate::gamestream::enet::Event;
        let Some(control) = self.control.as_mut() else { return false };
        let events = control.poll();
        if events.is_empty() {
            return false;
        }
        let session = self.srv.session.lock().unwrap().clone();
        for ev in events {
            match ev {
                Event::Connected => eprintln!("[control] *** CLIENT CONNECTED ***"),
                Event::Disconnected => {
                    eprintln!("[control] client disconnected");
                    self.sink = None;
                }
                Event::Message { channel, data } => {
                    if let Some(s) = &session {
                        crate::gamestream::control::on_message(s, channel, &data);
                    }
                }
            }
        }
        true
    }

    /// Hand one coded access unit to the client. Called with the same frames
    /// the SSE watchers get, so the picture is encoded once and consumed twice.
    pub fn feed_video(&mut self, frame: &EncodedFrame) {
        let session = self.srv.session.lock().unwrap().clone();
        let Some(session) = session else { return };
        if self.sink.is_none() {
            self.sink = Some(AuSink::new(session, Arc::new(match self.video.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[video] cannot clone the RTP socket: {e}");
                    return;
                }
            })));
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.emit(frame.data.clone());
        }
    }

    /// Drop the RTP sink when a session ends so the next one starts clean.
    pub fn end_session(&mut self) {
        self.sink = None;
        if let Some(c) = self.control.as_mut() {
            c.disconnect();
        }
    }
}

/// Read what is available, answer a complete request, flush what we can.
fn pump(c: &mut Conn, srv: &Arc<httpx::Server>, local_ip: &str, _tls_on: bool) -> bool {
    let mut busy = false;

    // TLS first: rustls owns the byte stream on the https port.
    if let Kind::Https(tls) = &mut c.kind {
        if tls.wants_read() {
            match tls.read_tls(&mut c.stream) {
                Ok(0) => c.closing = true,
                Ok(_) => {
                    busy = true;
                    if tls.process_new_packets().is_err() {
                        c.closing = true;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => c.closing = true,
            }
        }
        let mut plain = Vec::new();
        if tls.reader().read_to_end(&mut plain).is_ok() || !plain.is_empty() {
            c.rbuf.extend_from_slice(&plain);
        }
    } else {
        let mut buf = [0u8; 4096];
        loop {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.closing = true;
                    break;
                }
                Ok(n) => {
                    busy = true;
                    c.rbuf.extend_from_slice(&buf[..n]);
                    if c.rbuf.len() > MAX_REQUEST {
                        c.closing = true;
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    c.closing = true;
                    break;
                }
            }
        }
    }

    // A parked pairing request re-runs until the PIN lands.
    if let Some(path) = c.parked.clone() {
        let https = matches!(c.kind, Kind::Https(_));
        if let Outcome::Reply(body) = httpx::route(srv, &path, https, local_ip) {
            c.parked = None;
            queue_http(c, &body);
            busy = true;
        }
    } else if let Some(head_end) = find_head_end(&c.rbuf) {
        let head = String::from_utf8_lossy(&c.rbuf[..head_end]).to_string();
        c.rbuf.drain(..head_end);
        busy = true;
        match &c.kind {
            Kind::Rtsp => {
                // RTSP framing differs from HTTP; the ported handler owns it.
                let reply = crate::gamestream::rtsp::handle_raw(srv, &head);
                c.wbuf.extend_from_slice(reply.as_bytes());
                c.closing = false;
            }
            _ => {
                let https = matches!(c.kind, Kind::Https(_));
                // The client reached us on some address; the RTSP URL we hand
                // back has to be that one, not whatever we guessed at bind.
                let local_ip = c
                    .stream
                    .local_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_else(|_| local_ip.to_string());
                let local_ip = local_ip.as_str();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                match httpx::route(srv, &path, https, local_ip) {
                    Outcome::Reply(body) => queue_http(c, &body),
                    // Hold the connection open; the operator has 60s to supply
                    // the PIN. The bridge blocked a thread for this.
                    Outcome::AwaitPin => {
                        eprintln!("[pair] holding a pairing request until the PIN arrives");
                        c.parked = Some(path);
                    }
                }
            }
        }
    }

    // Flush.
    if !c.wbuf.is_empty() {
        let wrote = match &mut c.kind {
            Kind::Https(tls) => {
                let n = tls.writer().write(&c.wbuf).unwrap_or(0);
                let _ = tls.write_tls(&mut c.stream);
                n
            }
            _ => c.stream.write(&c.wbuf).unwrap_or(0),
        };
        if wrote > 0 {
            c.wbuf.drain(..wrote);
            busy = true;
            if c.wbuf.is_empty() {
                c.closing = true; // GameStream replies are Connection: close
            }
        }
    }
    busy
}

fn queue_http(c: &mut Conn, body: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    c.wbuf.extend_from_slice(resp.as_bytes());
}

/// End of the request head, CRLFCRLF or the bare-LF form some clients send.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).or_else(|| {
        buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
    })
}

/// A rustls server config under the pairing identity.
///
/// Client certificates are accepted at the TLS layer and checked ABOVE it:
/// Moonlight presents a self-signed certificate that no PKI can chain, and the
/// question that matters is "is this the certificate we paired with", which
/// only `PairState` can answer.
fn build_tls(srv: &httpx::Server) -> Result<rustls::ServerConfig, String> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer};
    use rsa::pkcs1::EncodeRsaPrivateKey;

    let cert = CertificateDer::from(srv.pair.cert_der().to_vec());
    let key_der = srv
        .pair
        .private_key()
        .to_pkcs1_der()
        .map_err(|e| format!("private key: {e}"))?;
    let key = PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(key_der.as_bytes().to_vec()));

    rustls::ServerConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| format!("tls identity: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both header terminators must be recognised: a client that sends bare
    /// LFs would otherwise hang until the header timeout and look like a
    /// network fault.
    #[test]
    fn the_head_terminator_is_found_in_both_forms() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\n\n"), Some(16));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
        assert_eq!(find_head_end(b""), None);
    }

    /// The reply must carry an accurate Content-Length; GameStream clients
    /// read exactly that many bytes and hang on a short count.
    #[test]
    fn the_response_length_matches_the_body() {
        let mut c = Conn {
            stream: TcpStream::connect("127.0.0.1:1").unwrap_or_else(|_| {
                // No connection needed; the test only inspects wbuf. Bind a
                // listener so connect() has something to accept.
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                let addr = l.local_addr().unwrap();
                let s = TcpStream::connect(addr).unwrap();
                let _ = l.accept();
                s
            }),
            kind: Kind::Http,
            rbuf: Vec::new(),
            wbuf: Vec::new(),
            since: Instant::now(),
            parked: None,
            closing: false,
        };
        queue_http(&mut c, "<root/>");
        let text = String::from_utf8_lossy(&c.wbuf).to_string();
        assert!(text.contains("Content-Length: 7"), "got: {text}");
        assert!(text.ends_with("<root/>"));
    }
}

/// Assemble the host: identity from object storage, control surface, sockets.
///
/// Called once, on the first turn after the machine is running. Two reasons it
/// is not built at boot: generating an RSA-2048 identity costs seconds on the
/// first ever run, and there is nothing to stream before the machine exists.
pub fn build(cfg: &crate::Config) -> Option<Host> {
    let ep = match crate::s3::Endpoint::parse(&cfg.endpoint, &cfg.region) {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!("[gs] no object storage ({e}); GameStream disabled");
            return None;
        }
    };
    let creds = cfg.config_creds.clone();
    let store = crate::gamestream::pair::S3Store::new(ep, cfg.bucket.clone(), creds);

    // The guest clock is the host's realtime source (see the boot log).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let pair = Arc::new(crate::gamestream::pair::PairState::load(Box::new(store), now));
    let srv = Arc::new(httpx::Server {
        pair,
        session: std::sync::Mutex::new(None),
        on_launch: Box::new(|_s| {}),
        host_name: cfg.title.clone(),
        unique_id: "0123456789ABCDEF".to_string(),
    });
    Host::bind(srv, "0.0.0.0".to_string())
}
