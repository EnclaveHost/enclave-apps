//! Enclave network-test — "who am I on the network?" (dedicated-IP identity)
//!
//! A run-mode wasip2 service app that demonstrates the platform's dedicated-IP
//! egress (phases 1 + 2) from the inside. It serves one plain-text status page
//! on BOTH of its declared ports and, per request, runs four probes:
//!
//!   [1] identity   — a hand-rolled SOCKS5 CONNECT to the ENCLAVE_EGRESS front; the
//!                    reply's BND.ADDR is the deployment's dedicated IPv6, as
//!                    derived by the enclave from the authenticated credential.
//!   [2] explicit   — over that same phase-1 tunnel, fetch an ip-echo service:
//!                    the INTERNET's view of the address we egress from.
//!   [3] transparent— the same fetch with completely UNMODIFIED std::net code
//!                    (no proxy, no SOCKS): under the phase-2 toolchain this is
//!                    silently routed through the same front, so it reports the
//!                    same address. This is the "you write nothing" proof.
//!   [4] lockdown   — dial 127.0.0.1:8080 (the supervisor, always listening
//!                    in-enclave). Under phase 2 the guest has no raw network,
//!                    so this MUST fail; if it connects, you are looking at the
//!                    pre-phase-2 (inherit-network) posture.
//!
//! On an egress-enabled enclave with the phase-2 toolchain, [1] == [2] == [3]
//! and [4] is denied: one stable IPv6, both directions (the same address serves
//! this page via the tcp6-relay), and no way to egress off-identity.
//!
//! The ONLY platform contract used: read ENCLAVE_PORTS, bind the actual ports.
//! Everything else is std.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Public, plain-HTTP ip-echo (returns just your address as text). Overridable
/// via ENCLAVE_CONFIG (a bare `host:port` string) for testing.
const ECHO: &str = "icanhazip.com:80";

fn main() {
    let ports = std::env::var("ENCLAVE_PORTS").unwrap_or_default();
    let actuals: Vec<u16> = ports
        .split(',')
        .filter_map(|e| e.split_once('=').and_then(|(_, a)| a.trim().parse().ok()))
        .collect();
    if actuals.is_empty() {
        eprintln!("fatal: ENCLAVE_PORTS is empty — this app must be launched by the wasm-manager");
        std::process::exit(1);
    }

    // One listener per assigned actual port (http:8000 -> the /x/<id> page,
    // tcp:7777 -> the same page over the dedicated-IP raw TCP path). No guest
    // threads on wasip2, so accept non-blockingly and round-robin the ports.
    let mut listeners = Vec::new();
    for port in &actuals {
        match TcpListener::bind(("127.0.0.1", *port)) {
            Ok(l) => {
                println!("[network-test] listening on 127.0.0.1:{port}");
                listeners.push(l);
            }
            Err(e) => eprintln!("[network-test] bind {port}: {e}"),
        }
    }
    if listeners.is_empty() {
        eprintln!("fatal: no port bound");
        std::process::exit(1);
    }
    if listeners.iter().any(|l| l.set_nonblocking(true).is_err()) {
        // no non-blocking accept on this runtime: serve the first port only
        eprintln!("[network-test] non-blocking accept unavailable; serving the first port only");
        let l = &listeners[0];
        let _ = l.set_nonblocking(false);
        loop {
            if let Ok((stream, _)) = l.accept() {
                let _ = stream.set_nonblocking(false);
                handle(stream);
            }
        }
    }

    loop {
        let mut idle = true;
        for l in &listeners {
            match l.accept() {
                Ok((stream, _)) => {
                    idle = false;
                    let _ = stream.set_nonblocking(false);
                    handle(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        }
        if idle {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn handle(mut s: TcpStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    // Drain the request line + headers (best effort — any request gets the page).
    let mut buf = [0u8; 2048];
    let _ = s.read(&mut buf);
    let body = report();
    let _ = write!(
        s,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

fn report() -> String {
    let mut out = String::new();
    out.push_str("== Enclave network-test — dedicated-IP identity ==\n\n");

    let ports = std::env::var("ENCLAVE_PORTS").unwrap_or_default();
    out.push_str(&format!("ports (ENCLAVE_PORTS):  {ports}\n"));

    let egress = std::env::var("ENCLAVE_EGRESS").ok();
    match &egress {
        Some(url) => out.push_str(&format!("egress (ENCLAVE_EGRESS): {}\n\n", redact(url))),
        None => out.push_str("egress (ENCLAVE_EGRESS): (not set — no phase-1 front advertised; [3]/[4] below still prove phase-2 when active)\n\n"),
    }

    let echo = echo_target();

    // [1] + [2] — phase 1: explicit SOCKS through the front. One CONNECT gives
    // us both the platform-attested identity (BND.ADDR) and the tunnel for the
    // internet-view fetch.
    let mut identity = None;
    match &egress {
        Some(url) => match socks_fetch(url, &echo) {
            Ok((bnd, seen)) => {
                identity = Some(bnd.clone());
                out.push_str(&format!("[1] identity (SOCKS BND.ADDR, platform-derived):   {bnd}\n"));
                out.push_str(&format!("[2] explicit egress fetch ({}):        internet sees {seen}\n", echo_host(&echo)));
            }
            Err(e) => out.push_str(&format!("[1][2] explicit egress probe failed: {e}\n")),
        },
        None => out.push_str("[1][2] skipped (no ENCLAVE_EGRESS)\n"),
    }

    // [3] — phase 2: the SAME fetch with plain std::net. Nothing here knows
    // about proxies; under the phase-2 toolchain the platform routes it anyway.
    match plain_fetch(&echo) {
        Ok(seen) => {
            out.push_str(&format!("[3] transparent fetch (unmodified std::net):        internet sees {seen}\n"));
            match &identity {
                Some(id) if *id == seen => out.push_str("    -> matches [1]: outbound is transparently source-tagged (phase 2 live)\n"),
                Some(_) => out.push_str("    -> differs from [1]: raw network still open (phase-1 toolchain, or local dev harness)\n"),
                None => {}
            }
        }
        Err(e) => out.push_str(&format!("[3] transparent fetch failed: {e}\n    -> with ENCLAVE_EGRESS set this means the raw network is closed but the front was unreachable\n")),
    }

    // [4] — the guardrail: with the phase-2 lockdown there is NO raw path, so a
    // loopback dial (the supervisor's own port) must be refused before it ever
    // leaves the sandbox. Connect-only; nothing is sent.
    out.push('\n');
    match TcpStream::connect("127.0.0.1:8080") {
        Err(e) => out.push_str(&format!("[4] loopback dial 127.0.0.1:8080: DENIED ({e}) — raw network closed \u{2713}\n")),
        Ok(mut c) => {
            // connected: can we actually exchange bytes, or is it a phantom?
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            let n = c.write(b"GET /health HTTP/1.0\r\n\r\n").and_then(|_| { let mut b = [0u8; 64]; c.read(&mut b) });
            out.push_str(&format!("[4] loopback dial 127.0.0.1:8080: CONNECTED (io: {n:?}) — pre-phase-2 posture (raw inherit-network still granted)\n"));
        }
    }
    // Known quirk, shown for app authors: a NON-BLOCKING connect_timeout can
    // report Ok for a denied dial; the socket then fails on first I/O — the
    // denial holds either way (no bytes ever flow).
    match TcpStream::connect_timeout(&"127.0.0.1:8080".parse().unwrap(), Duration::from_secs(3)) {
        Err(e) => out.push_str(&format!("[4b] same dial via connect_timeout: DENIED ({e})\n")),
        Ok(mut c) => {
            let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
            match c.write(b"GET /health HTTP/1.0\r\n\r\n").and_then(|_| { let mut b = [0u8; 8]; c.read(&mut b) }) {
                Err(e) => out.push_str(&format!("[4b] same dial via connect_timeout: phantom Ok, first I/O fails ({e}) — denial holds\n")),
                Ok(_) => out.push_str("[4b] same dial via connect_timeout: CONNECTED with I/O — raw network is open (pre-phase-2)\n"),
            }
        }
    }

    out.push_str("\nOn an egress-enabled enclave with the phase-2 toolchain: [1] == [2] == [3],\n");
    out.push_str("[4] denied — one stable IPv6 identity in both directions (this page is served\n");
    out.push_str("on that same address via the tcp6-relay), with no way to egress off-identity.\n");
    out
}

/// ENCLAVE_CONFIG may carry a bare `host:port` override for the echo service.
fn echo_target() -> String {
    match std::env::var("ENCLAVE_CONFIG") {
        Ok(c) if c.contains(':') && !c.contains('{') && !c.trim().is_empty() => c.trim().to_string(),
        _ => ECHO.to_string(),
    }
}

fn echo_host(target: &str) -> &str {
    target.rsplit_once(':').map(|(h, _)| h).unwrap_or(target)
}

/// Mask the credential in the ENCLAVE_EGRESS URL for display.
fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(s), Some(a)) if a > s + 3 => {
            let creds = &url[s + 3..a];
            let id = creds.split(':').next().unwrap_or("");
            format!("{}{}:****{}", &url[..s + 3], id, &url[a..])
        }
        _ => url.to_string(),
    }
}

/// Plain-std fetch of the ip-echo: resolve (prefer IPv6 — only a v6 destination
/// can carry the dedicated v6 source), connect, GET /, return the body text.
/// This is the code any app already has; phase 2 makes it source-tagged as-is.
fn plain_fetch(target: &str) -> Result<String, String> {
    let addrs: Vec<SocketAddr> = target
        .to_socket_addrs()
        .map_err(|e| format!("resolve: {e}"))?
        .collect();
    let addr = addrs
        .iter()
        .find(|a| matches!(a.ip(), IpAddr::V6(_)))
        .or_else(|| addrs.first())
        .ok_or("resolve: no addresses")?;
    let mut s = TcpStream::connect_timeout(addr, Duration::from_secs(8)).map_err(|e| format!("connect: {e}"))?;
    http_get(&mut s, echo_host(target))
}

/// Phase-1 probe: speak SOCKS5 (RFC 1928/1929) to the ENCLAVE_EGRESS front, CONNECT
/// to the echo service AS A DOMAIN (socks5h — the relay resolves it), read the
/// deployment's dedicated address from BND.ADDR, then fetch through the tunnel.
fn socks_fetch(egress_url: &str, target: &str) -> Result<(String, String), String> {
    let (id, token, front) = parse_egress(egress_url)?;
    let (host, port) = target.rsplit_once(':').ok_or("bad echo target")?;
    let port: u16 = port.parse().map_err(|_| "bad echo port")?;

    let mut s = TcpStream::connect(&front).map_err(|e| format!("front connect: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(20)));

    // greeting: user/pass auth required
    s.write_all(&[0x05, 0x01, 0x02]).map_err(es)?;
    let mut m = [0u8; 2];
    s.read_exact(&mut m).map_err(es)?;
    if m != [0x05, 0x02] {
        return Err("front rejected user/pass auth".into());
    }
    // auth: the per-deployment credential from ENCLAVE_EGRESS
    let mut auth = vec![0x01, id.len() as u8];
    auth.extend_from_slice(id.as_bytes());
    auth.push(token.len() as u8);
    auth.extend_from_slice(token.as_bytes());
    s.write_all(&auth).map_err(es)?;
    let mut a = [0u8; 2];
    s.read_exact(&mut a).map_err(es)?;
    if a[1] != 0x00 {
        return Err("credential rejected".into());
    }
    // CONNECT, ATYP=DOMAIN (remote resolution -> SSRF checked where DNS happens)
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).map_err(es)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).map_err(es)?;
    if head[1] != 0x00 {
        return Err(format!("CONNECT refused (rep={})", head[1]));
    }
    // BND.ADDR: the enclave front puts OUR derived source address here.
    let bnd = match head[3] {
        0x04 => {
            let mut b = [0u8; 18];
            s.read_exact(&mut b).map_err(es)?;
            let mut groups = [0u16; 8];
            for (i, g) in groups.iter_mut().enumerate() {
                *g = u16::from_be_bytes([b[i * 2], b[i * 2 + 1]]);
            }
            format!("{}", std::net::Ipv6Addr::from(groups))
        }
        0x01 => {
            let mut b = [0u8; 6];
            s.read_exact(&mut b).map_err(es)?;
            format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        }
        _ => return Err("unexpected BND.ADDR type".into()),
    };
    let seen = http_get(&mut s, host)?;
    Ok((bnd, seen))
}

/// Parse socks5h://<id>:<token>@<host>:<port> (the exact shape the supervisor
/// mints; id/token carry no ':' or '@' or percent-escapes).
fn parse_egress(url: &str) -> Result<(String, String, String), String> {
    let rest = url.split_once("://").map(|(_, r)| r).ok_or("bad ENCLAVE_EGRESS url")?;
    let (creds, hostport) = rest.rsplit_once('@').ok_or("no credential in ENCLAVE_EGRESS")?;
    let (id, token) = creds.split_once(':').ok_or("bad credential")?;
    Ok((id.into(), token.into(), hostport.into()))
}

/// Minimal HTTP/1.1 GET over an already-connected stream; returns the trimmed
/// body (the ip-echo answers with just the caller's address as text).
fn http_get(s: &mut TcpStream, host: &str) -> Result<String, String> {
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    write!(s, "GET / HTTP/1.1\r\nHost: {host}\r\nUser-Agent: network-test\r\nConnection: close\r\n\r\n").map_err(es)?;
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw); // tolerate srv-side close/timeouts: parse what arrived
    let text = String::from_utf8_lossy(&raw);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(&text);
    let ip = body.trim();
    if ip.is_empty() {
        return Err("empty echo body".into());
    }
    Ok(ip.to_string())
}

fn es(e: std::io::Error) -> String {
    e.to_string()
}
