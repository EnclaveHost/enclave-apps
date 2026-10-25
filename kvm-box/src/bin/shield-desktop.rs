//! Shield desktop controller: the portable Wasm (wasm32-wasip2) front for the
//! measured native desktop companion at the fixed loopback gateway.
//!
//! Single-threaded, nonblocking and bounded. Every request must carry HTTP Basic
//! `owner:<password>`; the password arrives only as KVMBOX_OWNER_PASSWORD from
//! secretrun's verified release and is kept solely as a SHA-256 digest.
#![forbid(unsafe_code)]

use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const HTML: &str = include_str!("shield-desktop.html");
const LISTEN: &str = "127.0.0.1:8000";
const COMPANION: &str = "127.0.0.1:19080";
/// Host sent upstream; never derived from the caller.
const UPSTREAM_HOST: &str = "127.0.0.1:19080";
const MAX_CONNS: usize = 32;
const BUF_CAP: usize = 64 * 1024;
const MAX_HEAD: usize = 16 * 1024;
const HEAD_SLACK: usize = 4096;
const MAX_HEADERS: usize = 64;
const MAX_UPSTREAM_HEADERS: usize = 96;
const MAX_BODY: u64 = 8 * 1024 * 1024;
const MAX_PASSWORD: usize = 1024;
const MAX_INTERIM: u8 = 8;
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_LIMIT: usize = 256 * 1024;
const PUMP_ROUNDS: usize = 4;
const ACCEPT_BURST: usize = 16;
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
const DASHBOARD_CSP: &str = "Content-Security-Policy: default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-ancestors 'none'\r\n";
const PLAIN_CSP: &str = "Content-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\n";
/// Only these caller headers reach the companion. Everything else (Authorization,
/// Proxy-Authorization, Forwarded, X-Forwarded-*, Cookie, Origin, hop-by-hop and
/// any internal identity header) is dropped by construction.
const FORWARDED_HEADERS: &[&str] = &[
    "accept", "accept-encoding", "accept-language", "cache-control", "content-type",
    "if-match", "if-modified-since", "if-none-match", "if-range", "if-unmodified-since",
    "pragma", "range", "user-agent", "x-compute-authorization", "x-kvm-box-request",
];
const SINGLETON_HEADERS: &[&str] = &[
    "host", "authorization", "content-length", "upgrade", "connection", "origin", "expect",
    "content-type", "range", "x-kvm-box-request", "x-compute-authorization",
    "sec-websocket-key", "sec-websocket-version", "sec-websocket-protocol",
];
const _: () = assert!(HTML.len() < BUF_CAP / 2);

struct Config {
    upstream: SocketAddr,
    owner_digest: [u8; 32],
    header_timeout: Duration,
    idle_timeout: Duration,
}

// ---------------------------------------------------------------- credentials

#[derive(Debug, PartialEq, Eq)]
enum CredentialError {
    LegacyHash,
    Missing,
    NotUtf8,
    Empty,
    TooLong,
}

impl CredentialError {
    fn message(&self) -> &'static str {
        match self {
            Self::LegacyHash => "KVMBOX_ACCESS_SHA256 is set; legacy baked hashes are refused",
            Self::Missing => "KVMBOX_OWNER_PASSWORD was not released",
            Self::NotUtf8 => "KVMBOX_OWNER_PASSWORD is not UTF-8",
            Self::Empty => "KVMBOX_OWNER_PASSWORD is empty",
            Self::TooLong => "KVMBOX_OWNER_PASSWORD exceeds 1024 bytes",
        }
    }
}

/// Pure loader: callers pass the values so tests never touch the process environment.
fn load_owner_digest(password: Option<OsString>, legacy_hash_present: bool) -> Result<[u8; 32], CredentialError> {
    if legacy_hash_present {
        if let Some(p) = password {
            wipe(&mut p.into_encoded_bytes());
        }
        return Err(CredentialError::LegacyHash);
    }
    let mut bytes = match password.ok_or(CredentialError::Missing)?.into_string() {
        Ok(s) => s.into_bytes(),
        Err(raw) => {
            wipe(&mut raw.into_encoded_bytes());
            return Err(CredentialError::NotUtf8);
        }
    };
    let result = if bytes.is_empty() {
        Err(CredentialError::Empty)
    } else if bytes.len() > MAX_PASSWORD {
        Err(CredentialError::TooLong)
    } else {
        Ok(sha256(&bytes))
    };
    wipe(&mut bytes);
    result
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

/// Best-effort scrub of secret-bearing buffers before they are freed.
fn wipe(bytes: &mut [u8]) {
    bytes.iter_mut().for_each(|b| *b = 0);
    std::hint::black_box(bytes);
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

fn owner_authorized(value: &[u8], owner_digest: &[u8; 32]) -> bool {
    let Some(encoded) = value.strip_prefix(b"Basic ") else { return false };
    let Ok(mut decoded) = STANDARD.decode(encoded) else { return false };
    let ok = match decoded.strip_prefix(b"owner:") {
        Some(p) if !p.is_empty() && p.len() <= MAX_PASSWORD => ct_eq(&sha256(p), owner_digest),
        _ => false,
    };
    wipe(&mut decoded);
    ok
}

// ------------------------------------------------------------------- helpers

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (s, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 20];
    for (chunk, v) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn websocket_accept(key: &[u8]) -> String {
    STANDARD.encode(sha1(&[key, WS_GUID].concat()))
}

fn values<'a>(headers: &[httparse::Header<'a>], name: &str) -> Vec<&'a [u8]> {
    headers.iter().filter(|h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value).collect()
}

fn trim(mut b: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = b {
        b = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = b {
        b = rest;
    }
    b
}

fn has_token(v: &[u8], token: &str) -> bool {
    v.split(|&b| b == b',').any(|t| trim(t).eq_ignore_ascii_case(token.as_bytes()))
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn protocol_list(v: &[u8]) -> Option<Vec<Vec<u8>>> {
    let list: Vec<Vec<u8>> = v.split(|&b| b == b',').map(|t| trim(t).to_vec()).collect();
    list.iter().all(|t| !t.is_empty() && t.iter().all(|&b| is_tchar(b))).then_some(list)
}

fn parse_length(v: &[u8]) -> Option<u64> {
    if v.is_empty() || v.len() > 19 || !v.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(v.iter().fold(0u64, |n, d| n * 10 + u64::from(d - b'0')))
}

fn valid_host(h: &[u8]) -> bool {
    !h.is_empty() && h.len() <= 255 && h.iter().all(|&b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
}

fn same_origin(origin: &[u8], host: &[u8]) -> bool {
    origin.strip_prefix(b"https://").or_else(|| origin.strip_prefix(b"http://")) == Some(host)
}

fn strict_crlf(raw: &[u8]) -> bool {
    raw.iter().enumerate().all(|(i, &b)| b != b'\n' || (i > 0 && raw[i - 1] == b'\r'))
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn target_chars_ok(part: &[u8], in_path: bool) -> bool {
    let mut i = 0;
    while i < part.len() {
        let c = part[i];
        if c == b'%' {
            let (Some(hi), Some(lo)) = (part.get(i + 1).and_then(|&b| hex_val(b)), part.get(i + 2).and_then(|&b| hex_val(b))) else {
                return false;
            };
            let v = hi * 16 + lo;
            if v < 0x20 || v == 0x7f {
                return false;
            }
            // Encoded separators, dots or unreserved bytes would let one route be spelled two ways.
            if in_path && (v.is_ascii_alphanumeric() || b"-._~/\\".contains(&v)) {
                return false;
            }
            i += 3;
        } else if c.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&c) || (!in_path && c == b'?') {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

fn valid_path(path: &str) -> bool {
    let segments: Vec<&str> = path[1..].split('/').collect();
    target_chars_ok(path.as_bytes(), true)
        && segments.iter().enumerate().all(|(i, s)| *s != "." && *s != ".." && (!s.is_empty() || i + 1 == segments.len()))
}

/// Origin-form only: no absolute-form, authority-form, `*`, fragments or ambiguous paths.
fn split_target(target: &str) -> Option<(&str, Option<&str>)> {
    if !target.starts_with('/') || target.contains('#') {
        return None;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    (valid_path(path) && query.map_or(true, |q| target_chars_ok(q.as_bytes(), false))).then_some((path, query))
}

fn moonlight_query_ok(query: Option<&str>) -> bool {
    let Some(query) = query else { return false };
    let (mut port, mut transport) = (None, None);
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("port", v)) if port.is_none() => port = Some(v),
            Some(("transport", v)) if transport.is_none() => transport = Some(v),
            _ => return false,
        }
    }
    matches!(
        (transport, port),
        (Some("tcp"), Some("47984" | "47989" | "48010")) | (Some("udp"), Some("47998" | "47999" | "48000"))
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Response",
    }
}

fn local(status: u16, mime: &str, body: &[u8], extra_headers: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n{extra_headers}\r\n",
        reason(status),
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn plain(status: u16, msg: &str) -> Vec<u8> {
    local(status, "text/plain; charset=utf-8", msg.as_bytes(), PLAIN_CSP)
}

fn unauthorized() -> Vec<u8> {
    let extra = format!("WWW-Authenticate: Basic realm=\"KVM Box\", charset=\"UTF-8\"\r\n{PLAIN_CSP}");
    local(401, "text/plain; charset=utf-8", b"Sign in as owner.", &extra)
}

// ------------------------------------------------------------ request policy

#[derive(Debug)]
enum Kind {
    Plain,
    WebSocket { accept: String, protocols: Vec<Vec<u8>> },
    Moonlight,
}

struct Plan {
    head: Vec<u8>,
    prefix: Vec<u8>,
    body_len: u64,
    expect_continue: bool,
    kind: Kind,
    early: Vec<u8>,
    desktop: bool,
}

enum Decision {
    Local(Vec<u8>),
    Proxy(Plan),
}

enum Route<'a> {
    Dashboard,
    Api(&'static str, &'static str),
    Desktop(&'a str),
    WebSocket(&'a str),
    Moonlight(&'a str),
}

fn refuse(status: u16, msg: &str) -> Decision {
    Decision::Local(plain(status, msg))
}

fn upstream_line(method: &str, rest: &str, query: Option<&str>) -> Vec<u8> {
    let mut line = format!("{method} {rest}");
    if let Some(q) = query {
        line.push('?');
        line.push_str(q);
    }
    line.push_str(" HTTP/1.1\r\nHost: ");
    line.push_str(UPSTREAM_HOST);
    line.push_str("\r\n");
    line.into_bytes()
}

fn push_forwarded(head: &mut Vec<u8>, headers: &[httparse::Header]) {
    for h in headers.iter().filter(|h| FORWARDED_HEADERS.iter().any(|n| h.name.eq_ignore_ascii_case(n))) {
        head.extend_from_slice(h.name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(h.value);
        head.extend_from_slice(b"\r\n");
    }
}

/// Pure policy for one complete request head. `extra` holds bytes the client sent
/// after the head that were already read. Nothing here can dial upstream, so every
/// refusal (including authentication) happens before any companion connection.
fn decide(raw: &[u8], extra: &[u8], owner_digest: &[u8; 32]) -> Decision {
    let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut slots);
    match req.parse(raw) {
        Ok(httparse::Status::Complete(n)) if n == raw.len() && strict_crlf(raw) => {}
        Err(httparse::Error::TooManyHeaders) => return refuse(431, "Too many request headers"),
        _ => return refuse(400, "Malformed request"),
    }
    let (method, target, version) = (req.method.unwrap_or(""), req.path.unwrap_or(""), req.version);
    let headers = &*req.headers;

    let auth = values(headers, "authorization");
    if auth.len() != 1 || !owner_authorized(auth[0], owner_digest) {
        return Decision::Local(unauthorized());
    }
    if version != Some(1) {
        return refuse(505, "HTTP/1.1 required");
    }
    if headers.iter().any(|h| h.value.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f)) {
        return refuse(400, "Invalid header value");
    }
    if SINGLETON_HEADERS.iter().any(|name| values(headers, name).len() > 1) {
        return refuse(400, "Duplicate request header");
    }
    let host = match values(headers, "host").as_slice() {
        [h] if valid_host(h) => *h,
        _ => return refuse(400, "Invalid Host"),
    };
    if !values(headers, "transfer-encoding").is_empty() {
        return refuse(400, "Transfer-Encoding is not supported");
    }
    let body_len = match values(headers, "content-length").first() {
        None => 0,
        Some(v) => match parse_length(v) {
            Some(n) => n,
            None => return refuse(400, "Invalid Content-Length"),
        },
    };
    if body_len > MAX_BODY {
        return refuse(413, "Request body too large");
    }
    if let Some(origin) = values(headers, "origin").first() {
        if !same_origin(origin, host) {
            return refuse(403, "Cross-origin request refused");
        }
    }
    let Some((path, query)) = split_target(target) else { return refuse(400, "Invalid request target") };
    let upgrade = values(headers, "upgrade").first().copied();
    let connection_upgrade = values(headers, "connection").first().is_some_and(|v| has_token(v, "upgrade"));
    if upgrade.is_some() != connection_upgrade {
        return refuse(400, "Conflicting upgrade headers");
    }
    if body_len > 0 && method != "POST" {
        return refuse(400, "Request body requires POST");
    }
    let expect_continue = match values(headers, "expect").first() {
        None => false,
        Some(v) if v.eq_ignore_ascii_case(b"100-continue") => body_len > 0,
        Some(_) => return refuse(417, "Expectation not supported"),
    };

    let route = match path {
        "/" => Route::Dashboard,
        "/api/status" => Route::Api("GET", "status"),
        "/api/start" => Route::Api("POST", "start"),
        "/api/stop" => Route::Api("POST", "stop"),
        p => match p.strip_prefix("/desktop") {
            // /v1 is the controller's action namespace; it is reachable only via /api.
            Some(rest) if rest.starts_with('/') && rest != "/v1" && !rest.starts_with("/v1/") => match rest {
                "/websockify" => Route::WebSocket(rest),
                "/api/moonlight/tunnel" => Route::Moonlight(rest),
                _ => Route::Desktop(rest),
            },
            _ => return refuse(404, "Not found"),
        },
    };
    let tunnel = matches!(route, Route::WebSocket(_) | Route::Moonlight(_));
    if upgrade.is_some() && !tunnel {
        return refuse(400, "Upgrade is not allowed on this route");
    }
    if !tunnel && extra.len() as u64 > body_len {
        return refuse(400, "Pipelined requests are not supported");
    }

    match route {
        Route::Dashboard => {
            if method != "GET" {
                return refuse(405, "GET only");
            }
            Decision::Local(local(200, "text/html; charset=utf-8", HTML.as_bytes(), DASHBOARD_CSP))
        }
        Route::Api(want, action) => {
            if method != want {
                return refuse(405, "Method not allowed");
            }
            if query.is_some() {
                return refuse(400, "Query not supported");
            }
            if body_len != 0 {
                return refuse(400, "Request bodies are not supported");
            }
            if want == "POST" && !matches!(values(headers, "x-kvm-box-request").as_slice(), [v] if *v == &b"1"[..]) {
                return refuse(403, "Explicit action header required");
            }
            let length = if want == "POST" { "X-KVM-Box-Request: 1\r\nContent-Length: 0\r\n" } else { "" };
            let head = format!("{want} /v1/{action} HTTP/1.1\r\nHost: {UPSTREAM_HOST}\r\nAccept: application/json\r\n{length}Connection: close\r\n\r\n");
            Decision::Proxy(Plan { head: head.into_bytes(), prefix: Vec::new(), body_len: 0, expect_continue: false, kind: Kind::Plain, early: Vec::new(), desktop: false })
        }
        Route::Desktop(rest) => {
            if !matches!(method, "GET" | "HEAD" | "POST") {
                return refuse(405, "Method not allowed");
            }
            let mut head = upstream_line(method, rest, query);
            push_forwarded(&mut head, headers);
            if method == "POST" {
                head.extend_from_slice(format!("Content-Length: {body_len}\r\n").as_bytes());
            }
            head.extend_from_slice(b"Connection: close\r\n\r\n");
            Decision::Proxy(Plan { head, prefix: extra.to_vec(), body_len, expect_continue, kind: Kind::Plain, early: Vec::new(), desktop: true })
        }
        Route::WebSocket(rest) => {
            if query.is_some() {
                return refuse(400, "WebSocket query not supported");
            }
            if method != "GET" {
                return refuse(405, "GET only");
            }
            if !upgrade.is_some_and(|u| u.eq_ignore_ascii_case(b"websocket")) {
                return refuse(400, "WebSocket upgrade required");
            }
            if body_len != 0 || !extra.is_empty() {
                return refuse(400, "Unexpected data before WebSocket handshake");
            }
            if !matches!(values(headers, "sec-websocket-version").as_slice(), [v] if *v == &b"13"[..]) {
                return refuse(400, "Sec-WebSocket-Version 13 required");
            }
            let key = match values(headers, "sec-websocket-key").as_slice() {
                [k] if STANDARD.decode(k).is_ok_and(|d| d.len() == 16) => *k,
                _ => return refuse(400, "Invalid Sec-WebSocket-Key"),
            };
            let offered = values(headers, "sec-websocket-protocol").first().copied();
            let protocols = match offered.map(protocol_list) {
                None => Vec::new(),
                Some(Some(list)) => list,
                Some(None) => return refuse(400, "Invalid Sec-WebSocket-Protocol"),
            };
            let mut head = upstream_line("GET", rest, query);
            head.extend_from_slice(b"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: ");
            head.extend_from_slice(key);
            head.extend_from_slice(b"\r\n");
            if let Some(p) = offered {
                head.extend_from_slice(b"Sec-WebSocket-Protocol: ");
                head.extend_from_slice(p);
                head.extend_from_slice(b"\r\n");
            }
            push_forwarded(&mut head, headers);
            head.extend_from_slice(b"\r\n");
            let kind = Kind::WebSocket { accept: websocket_accept(key), protocols };
            Decision::Proxy(Plan { head, prefix: Vec::new(), body_len: 0, expect_continue: false, kind, early: Vec::new(), desktop: true })
        }
        Route::Moonlight(rest) => {
            if method != "GET" {
                return refuse(405, "GET only");
            }
            if upgrade != Some(b"kvm-box-tunnel".as_slice()) || body_len != 0 {
                return refuse(400, "Moonlight tunnel upgrade required");
            }
            if !moonlight_query_ok(query) {
                return refuse(400, "Invalid tunnel");
            }
            let mut head = upstream_line("GET", rest, query);
            head.extend_from_slice(b"Upgrade: kvm-box-tunnel\r\nConnection: Upgrade\r\n");
            push_forwarded(&mut head, headers);
            head.extend_from_slice(b"\r\n");
            // Early tunnel bytes are held until a validated 101; dropped otherwise.
            Decision::Proxy(Plan { head, prefix: Vec::new(), body_len: 0, expect_continue: false, kind: Kind::Moonlight, early: extra.to_vec(), desktop: true })
        }
    }
}

// ----------------------------------------------------------- response policy

enum RespStep {
    More,
    Interim(usize),
    Switch,
    Final(Vec<u8>, usize),
}

fn rewrite_location(v: &[u8], desktop: bool) -> Option<Vec<u8>> {
    if !desktop || v.starts_with(b"//") || v.contains(&b'\\') {
        return None;
    }
    if v.starts_with(b"/") {
        return Some([b"/desktop".as_slice(), v].concat());
    }
    let first = v.iter().position(|b| matches!(b, b'/' | b'?' | b'#')).unwrap_or(v.len());
    (!v[..first].contains(&b':')).then(|| v.to_vec())
}

fn final_head(code: u16, reason: &str, headers: &[httparse::Header], desktop: bool) -> Vec<u8> {
    let nominated: Vec<String> = values(headers, "connection")
        .iter()
        .flat_map(|v| v.split(|&b| b == b',').map(|t| String::from_utf8_lossy(trim(t)).to_ascii_lowercase()))
        .collect();
    let mut out = format!("HTTP/1.1 {code} {reason}\r\n").into_bytes();
    for h in headers {
        let name = h.name.to_ascii_lowercase();
        if matches!(name.as_str(), "connection" | "keep-alive" | "proxy-connection" | "upgrade") || nominated.contains(&name) {
            continue;
        }
        if desktop && matches!(name.as_str(), "x-content-type-options" | "x-frame-options") {
            continue;
        }
        let value = if name == "location" {
            match rewrite_location(h.value, desktop) {
                Some(v) => v,
                None => continue,
            }
        } else {
            h.value.to_vec()
        };
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&value);
        out.extend_from_slice(b"\r\n");
    }
    if desktop {
        out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\nX-Frame-Options: SAMEORIGIN\r\nContent-Security-Policy: frame-ancestors 'self'\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

/// Validates the companion's response head. A 101 is accepted only for the
/// protocol this request asked for, with that protocol's handshake proof.
fn classify_response(buf: &[u8], kind: &Kind, desktop: bool) -> Result<RespStep, &'static str> {
    let mut slots = [httparse::EMPTY_HEADER; MAX_UPSTREAM_HEADERS];
    let mut resp = httparse::Response::new(&mut slots);
    let n = match resp.parse(buf) {
        Ok(httparse::Status::Complete(n)) if n <= MAX_HEAD => n,
        Ok(httparse::Status::Partial) if buf.len() <= MAX_HEAD => return Ok(RespStep::More),
        _ => return Err("Invalid desktop companion response"),
    };
    let (code, reason) = (resp.code.unwrap_or(0), resp.reason.unwrap_or(""));
    let headers = &*resp.headers;
    let connection_upgrade = values(headers, "connection").iter().any(|v| has_token(v, "upgrade"));
    let upgrade_is = |want: &str| connection_upgrade && matches!(values(headers, "upgrade").as_slice(), [u] if u.eq_ignore_ascii_case(want.as_bytes()));
    match (code, kind) {
        (101, Kind::Plain) => Err("Unsolicited upgrade from desktop companion"),
        (101, Kind::Moonlight) if upgrade_is("kvm-box-tunnel") => Ok(RespStep::Switch),
        (101, Kind::WebSocket { accept, protocols })
            if upgrade_is("websocket")
                && matches!(values(headers, "sec-websocket-accept").as_slice(), [a] if *a == accept.as_bytes())
                && values(headers, "sec-websocket-extensions").is_empty()
                && match values(headers, "sec-websocket-protocol").as_slice() {
                    [] => true,
                    [p] => protocols.iter().any(|o| o.as_slice() == *p),
                    _ => false,
                } =>
        {
            Ok(RespStep::Switch)
        }
        (101, _) => Err("Invalid upgrade response from desktop companion"),
        (100..=199, _) => Ok(RespStep::Interim(n)),
        (200..=599, _) => Ok(RespStep::Final(final_head(code, reason, headers, desktop), n)),
        _ => Err("Invalid desktop companion status"),
    }
}

// --------------------------------------------------------------- connections

#[derive(Debug)]
enum Io {
    Moved(usize),
    Blocked,
    Eof,
    Failed,
}

fn retry(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

/// Fixed-capacity byte queue for one direction of one connection.
struct Buf {
    data: Box<[u8]>,
    start: usize,
    end: usize,
}

impl Buf {
    fn new() -> Self {
        Buf { data: vec![0u8; BUF_CAP].into_boxed_slice(), start: 0, end: 0 }
    }
    fn len(&self) -> usize {
        self.end - self.start
    }
    fn is_empty(&self) -> bool {
        self.start == self.end
    }
    fn space(&self) -> usize {
        self.data.len() - self.len()
    }
    fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }
    fn compact(&mut self) {
        if self.start > 0 {
            self.data.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
    }
    fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > self.space() {
            return false;
        }
        if self.data.len() - self.end < bytes.len() {
            self.compact();
        }
        self.data[self.end..self.end + bytes.len()].copy_from_slice(bytes);
        self.end += bytes.len();
        true
    }
    fn fill_from<R: Read>(&mut self, src: &mut R, limit: usize) -> Io {
        if self.end == self.data.len() {
            self.compact();
        }
        let n = limit.min(self.data.len() - self.end);
        if n == 0 {
            return Io::Blocked;
        }
        match src.read(&mut self.data[self.end..self.end + n]) {
            Ok(0) => Io::Eof,
            Ok(k) => {
                self.end += k;
                Io::Moved(k)
            }
            Err(e) if retry(&e) => Io::Blocked,
            Err(_) => Io::Failed,
        }
    }
    fn drain_to<W: Write>(&mut self, dst: &mut W) -> Io {
        if self.is_empty() {
            return Io::Blocked;
        }
        match dst.write(&self.data[self.start..self.end]) {
            Ok(0) => Io::Failed,
            Ok(k) => {
                self.start += k;
                if self.start == self.end {
                    self.clear();
                }
                Io::Moved(k)
            }
            Err(e) if retry(&e) => Io::Blocked,
            Err(_) => Io::Failed,
        }
    }
}

fn read_vec<R: Read>(vec: &mut Vec<u8>, src: &mut R, cap: usize) -> Io {
    let mut chunk = [0u8; 4096];
    let room = cap.saturating_sub(vec.len()).min(chunk.len());
    if room == 0 {
        return Io::Failed;
    }
    match src.read(&mut chunk[..room]) {
        Ok(0) => Io::Eof,
        Ok(n) => {
            vec.extend_from_slice(&chunk[..n]);
            Io::Moved(n)
        }
        Err(e) if retry(&e) => Io::Blocked,
        Err(_) => Io::Failed,
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Head,
    Local,
    Proxy,
    Tunnel,
    Drain,
    Done,
}

struct Conn {
    client: TcpStream,
    upstream: Option<TcpStream>,
    phase: Phase,
    deadline: Instant,
    head: Vec<u8>,
    kind: Kind,
    desktop: bool,
    c2u: Buf,
    u2c: Buf,
    resp_head: Vec<u8>,
    resp_started: bool,
    interim: u8,
    early: Vec<u8>,
    body_left: u64,
    client_eof: bool,
    upstream_eof: bool,
    client_shut: bool,
    upstream_shut: bool,
    drained: usize,
}

impl Conn {
    fn new(client: TcpStream, now: Instant, cfg: &Config) -> Option<Conn> {
        client.set_nonblocking(true).ok()?;
        Some(Conn {
            client,
            upstream: None,
            phase: Phase::Head,
            deadline: now + cfg.header_timeout,
            head: Vec::new(),
            kind: Kind::Plain,
            desktop: true,
            c2u: Buf::new(),
            u2c: Buf::new(),
            resp_head: Vec::new(),
            resp_started: false,
            interim: 0,
            early: Vec::new(),
            body_left: 0,
            client_eof: false,
            upstream_eof: false,
            client_shut: false,
            upstream_shut: false,
            drained: 0,
        })
    }

    fn pump(&mut self, now: Instant, cfg: &Config) -> bool {
        let mut progress = false;
        for _ in 0..PUMP_ROUNDS {
            let moved = match self.phase {
                Phase::Head => self.pump_head(now, cfg),
                Phase::Local => self.pump_local(now),
                Phase::Proxy => self.pump_proxy(now),
                Phase::Tunnel => self.pump_tunnel(),
                Phase::Drain => self.pump_drain(),
                Phase::Done => false,
            };
            if !moved {
                break;
            }
            progress = true;
            if matches!(self.phase, Phase::Proxy | Phase::Tunnel) {
                self.deadline = now + cfg.idle_timeout;
            }
        }
        if self.phase != Phase::Done && now >= self.deadline {
            self.expire(now);
            progress = true;
        }
        progress
    }

    fn expire(&mut self, now: Instant) {
        match self.phase {
            Phase::Head => self.respond(408, "Request header timeout", now),
            Phase::Proxy if !self.resp_started => self.respond(504, "Desktop companion timed out", now),
            _ => self.close(),
        }
    }

    fn close(&mut self) {
        wipe(&mut self.head);
        self.upstream = None;
        self.phase = Phase::Done;
    }

    fn respond(&mut self, status: u16, msg: &str, now: Instant) {
        let resp = if self.desktop {
            plain(status, msg)
        } else {
            local(status, "application/json", serde_json::json!({ "error": msg }).to_string().as_bytes(), PLAIN_CSP)
        };
        self.respond_raw(resp, now);
    }

    fn respond_raw(&mut self, resp: Vec<u8>, now: Instant) {
        wipe(&mut self.head);
        self.upstream = None;
        if self.u2c.push(&resp) {
            self.phase = Phase::Local;
            self.deadline = now + WRITE_TIMEOUT;
        } else {
            self.close();
        }
    }

    /// Response complete: close our write side, then discard (never forward) any
    /// further client bytes briefly so the close does not reset the response.
    fn finish_response(&mut self, now: Instant) {
        self.upstream = None;
        let _ = self.client.shutdown(Shutdown::Write);
        self.phase = Phase::Drain;
        self.deadline = now + DRAIN_TIMEOUT;
    }

    fn pump_head(&mut self, now: Instant, cfg: &Config) -> bool {
        match read_vec(&mut self.head, &mut self.client, MAX_HEAD + HEAD_SLACK) {
            Io::Moved(_) => {}
            Io::Blocked => return false,
            Io::Eof | Io::Failed => {
                self.close();
                return true;
            }
        }
        let end = match find_head_end(&self.head) {
            Some(end) if end <= MAX_HEAD => end,
            Some(_) => {
                self.respond_raw(plain(431, "Request header too large"), now);
                return true;
            }
            None => {
                if self.head.len() > MAX_HEAD {
                    self.respond_raw(plain(431, "Request header too large"), now);
                }
                return true;
            }
        };
        let extra = self.head.split_off(end);
        let decision = decide(&self.head, &extra, &cfg.owner_digest);
        wipe(&mut self.head);
        self.head = Vec::new();
        match decision {
            Decision::Local(resp) => self.respond_raw(resp, now),
            Decision::Proxy(plan) => self.start_proxy(plan, now, cfg),
        }
        true
    }

    fn start_proxy(&mut self, plan: Plan, now: Instant, cfg: &Config) {
        self.desktop = plan.desktop;
        // Bound even a saturated loopback accept queue. The destination is fixed.
        let upstream = match TcpStream::connect_timeout(&cfg.upstream, CONNECT_TIMEOUT) {
            Ok(s) if s.set_nonblocking(true).is_ok() => s,
            _ => return self.respond(502, "Desktop companion unavailable", now),
        };
        let _ = upstream.set_nodelay(true);
        let _ = self.client.set_nodelay(true);
        if !self.c2u.push(&plan.head) || !self.c2u.push(&plan.prefix) {
            return self.respond(431, "Request header too large", now);
        }
        self.body_left = plan.body_len - plan.prefix.len() as u64;
        if plan.expect_continue && self.body_left > 0 {
            self.u2c.push(CONTINUE);
        }
        self.kind = plan.kind;
        self.early = plan.early;
        self.upstream = Some(upstream);
        self.phase = Phase::Proxy;
        self.deadline = now + cfg.idle_timeout;
    }

    fn pump_local(&mut self, now: Instant) -> bool {
        if self.u2c.is_empty() {
            self.finish_response(now);
            return true;
        }
        match self.u2c.drain_to(&mut self.client) {
            Io::Moved(_) => {
                if self.u2c.is_empty() {
                    self.finish_response(now);
                }
                true
            }
            Io::Blocked => false,
            Io::Eof | Io::Failed => {
                self.close();
                true
            }
        }
    }

    fn pump_drain(&mut self) -> bool {
        let mut scratch = [0u8; 4096];
        match self.client.read(&mut scratch) {
            Ok(0) => self.close(),
            Ok(n) => {
                self.drained += n;
                if self.drained > DRAIN_LIMIT {
                    self.close();
                }
            }
            Err(e) if retry(&e) => return false,
            Err(_) => self.close(),
        }
        true
    }

    fn pump_proxy(&mut self, now: Instant) -> bool {
        let mut moved = false;
        // Request body: exactly the declared remaining length, never further client bytes.
        if self.body_left > 0 {
            let limit = self.body_left.min(self.c2u.space() as u64) as usize;
            match self.c2u.fill_from(&mut self.client, limit) {
                Io::Moved(n) => {
                    self.body_left -= n as u64;
                    moved = true;
                }
                Io::Blocked => {}
                // A truncated body must never reach the companion as a complete request.
                Io::Eof | Io::Failed => {
                    self.close();
                    return true;
                }
            }
        }
        let sent = match self.upstream.as_mut() {
            Some(up) => self.c2u.drain_to(up),
            None => Io::Failed,
        };
        match sent {
            Io::Moved(_) => moved = true,
            Io::Blocked => {}
            Io::Eof | Io::Failed if !self.resp_started => {
                self.respond(502, "Desktop companion closed the request", now);
                return true;
            }
            Io::Eof | Io::Failed => {
                self.body_left = 0;
                self.c2u.clear();
            }
        }
        if !self.resp_started {
            let got = match self.upstream.as_mut() {
                Some(up) => read_vec(&mut self.resp_head, up, MAX_HEAD + HEAD_SLACK),
                None => Io::Failed,
            };
            match got {
                Io::Moved(_) => {
                    moved = true;
                    self.advance_response(now);
                    if self.phase != Phase::Proxy {
                        return true;
                    }
                }
                Io::Blocked => {}
                Io::Eof | Io::Failed => {
                    self.respond(502, "Desktop companion sent no response", now);
                    return true;
                }
            }
        } else if !self.upstream_eof {
            let got = match self.upstream.as_mut() {
                Some(up) => self.u2c.fill_from(up, usize::MAX),
                None => Io::Failed,
            };
            match got {
                Io::Moved(_) => moved = true,
                Io::Blocked => {}
                Io::Eof => {
                    self.upstream_eof = true;
                    moved = true;
                }
                Io::Failed => {
                    self.close();
                    return true;
                }
            }
        }
        match self.u2c.drain_to(&mut self.client) {
            Io::Moved(_) => moved = true,
            Io::Blocked => {}
            Io::Eof | Io::Failed => {
                self.close();
                return true;
            }
        }
        if self.resp_started && self.upstream_eof && self.u2c.is_empty() {
            self.finish_response(now);
            moved = true;
        }
        moved
    }

    fn advance_response(&mut self, now: Instant) {
        loop {
            match classify_response(&self.resp_head, &self.kind, self.desktop) {
                Ok(RespStep::More) => return,
                Ok(RespStep::Interim(n)) => {
                    self.interim += 1;
                    if self.interim > MAX_INTERIM {
                        return self.respond(502, "Too many interim responses", now);
                    }
                    self.resp_head.drain(..n);
                }
                Ok(RespStep::Final(head, n)) => {
                    let rest = self.resp_head.split_off(n);
                    self.resp_head = Vec::new();
                    self.early.clear();
                    if !self.u2c.push(&head) || !self.u2c.push(&rest) {
                        return self.respond(502, "Desktop companion response too large", now);
                    }
                    self.resp_started = true;
                    return;
                }
                Ok(RespStep::Switch) => {
                    let switched = std::mem::take(&mut self.resp_head);
                    let early = std::mem::take(&mut self.early);
                    if !self.u2c.push(&switched) || !self.c2u.push(&early) {
                        return self.respond(502, "Desktop companion response too large", now);
                    }
                    self.resp_started = true;
                    self.phase = Phase::Tunnel;
                    return;
                }
                Err(msg) => return self.respond(502, msg, now),
            }
        }
    }

    /// Opaque full duplex with half-close propagation; each side has its own bounded queue.
    fn pump_tunnel(&mut self) -> bool {
        let Some(up) = self.upstream.as_mut() else {
            self.close();
            return true;
        };
        let mut moved = false;
        let mut fail = false;
        if !self.client_eof && self.c2u.space() > 0 {
            match self.c2u.fill_from(&mut self.client, usize::MAX) {
                Io::Moved(_) => moved = true,
                Io::Blocked => {}
                Io::Eof => {
                    self.client_eof = true;
                    moved = true;
                }
                Io::Failed => fail = true,
            }
        }
        if !fail {
            match self.c2u.drain_to(up) {
                Io::Moved(_) => moved = true,
                Io::Blocked => {}
                Io::Eof | Io::Failed => fail = true,
            }
        }
        if !fail && self.client_eof && self.c2u.is_empty() && !self.upstream_shut {
            fail = up.shutdown(Shutdown::Write).is_err();
            self.upstream_shut = true;
            moved = true;
        }
        if !fail && !self.upstream_eof && self.u2c.space() > 0 {
            match self.u2c.fill_from(up, usize::MAX) {
                Io::Moved(_) => moved = true,
                Io::Blocked => {}
                Io::Eof => {
                    self.upstream_eof = true;
                    moved = true;
                }
                Io::Failed => fail = true,
            }
        }
        if !fail {
            match self.u2c.drain_to(&mut self.client) {
                Io::Moved(_) => moved = true,
                Io::Blocked => {}
                Io::Eof | Io::Failed => fail = true,
            }
        }
        if !fail && self.upstream_eof && self.u2c.is_empty() && !self.client_shut {
            fail = self.client.shutdown(Shutdown::Write).is_err();
            self.client_shut = true;
            moved = true;
        }
        if fail || (self.client_shut && self.upstream_shut) {
            self.close();
            return true;
        }
        moved
    }
}

fn refuse_busy(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(true);
    let _ = stream.write(&plain(503, "Too many connections"));
}

/// Single-threaded event loop. Private so tests can inject a listener, companion
/// address and short deadlines; production passes the fixed constants.
fn serve(listener: &TcpListener, cfg: &Config, stop: &AtomicBool) {
    let mut conns: Vec<Conn> = Vec::with_capacity(MAX_CONNS);
    let mut idle_rounds: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let mut progress = false;
        for _ in 0..ACCEPT_BURST {
            match listener.accept() {
                Ok((stream, _)) => {
                    progress = true;
                    if conns.len() >= MAX_CONNS {
                        // An idle unauthenticated peer must not reserve a slot
                        // against a new owner request. Established authorized
                        // streams are never evicted by new connections.
                        if let Some(index) = conns.iter().enumerate()
                            .filter(|(_, conn)| conn.phase == Phase::Head)
                            .min_by_key(|(_, conn)| conn.deadline).map(|(index, _)| index) {
                            conns.swap_remove(index).close();
                        }
                    }
                    if conns.len() >= MAX_CONNS {
                        refuse_busy(stream);
                    } else if let Some(conn) = Conn::new(stream, now, cfg) {
                        conns.push(conn);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        for conn in conns.iter_mut() {
            progress |= conn.pump(now, cfg);
        }
        conns.retain(|c| c.phase != Phase::Done);
        if progress {
            idle_rounds = 0;
        } else {
            idle_rounds = idle_rounds.saturating_add(1);
            std::thread::sleep(if idle_rounds < 100 { Duration::from_micros(500) } else { Duration::from_millis(2) });
        }
    }
}

fn main() {
    let legacy = std::env::var_os("KVMBOX_ACCESS_SHA256").is_some();
    let password = std::env::var_os("KVMBOX_OWNER_PASSWORD");
    // Single-threaded startup: nothing else can observe the environment while it is cleared.
    std::env::remove_var("KVMBOX_OWNER_PASSWORD");
    let owner_digest = match load_owner_digest(password, legacy) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Shield desktop controller refused to start: {}", e.message());
            std::process::exit(1);
        }
    };
    let cfg = Config {
        upstream: COMPANION.parse().expect("companion address"),
        owner_digest,
        header_timeout: HEADER_TIMEOUT,
        idle_timeout: IDLE_TIMEOUT,
    };
    let listener = match TcpListener::bind(LISTEN).and_then(|l| l.set_nonblocking(true).map(|()| l)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Shield desktop controller cannot listen: {e}");
            std::process::exit(1);
        }
    };
    println!("Shield desktop controller listening on 8000");
    serve(&listener, &cfg, &AtomicBool::new(false));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Arc, Mutex};
    use std::thread;

    // Public synthetic fixtures only; no real credential is read.
    const PASSWORD: &str = "synthetic-owner-passphrase-0001";
    const HOST: &str = "desk.example:8443";
    const ML_101: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\n\r\n";
    const FLOOD: usize = 4 << 20;

    fn digest() -> [u8; 32] {
        sha256(PASSWORD.as_bytes())
    }
    fn auth() -> String {
        format!("Authorization: Basic {}\r\n", STANDARD.encode(format!("owner:{PASSWORD}")))
    }
    fn request(method: &str, target: &str, headers: &str) -> String {
        format!("{method} {target} HTTP/1.1\r\nHost: {HOST}\r\n{}{headers}\r\n", auth())
    }
    fn ws_with(version: &str, key: &str) -> String {
        request(
            "GET",
            "/desktop/websockify",
            &format!("Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: {version}\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Protocol: binary\r\nOrigin: https://{HOST}\r\n"),
        )
    }
    fn ws_request() -> String {
        ws_with("13", "dGhlIHNhbXBsZSBub25jZQ==")
    }
    fn ml_request(query: &str) -> String {
        request("GET", &format!("/desktop/api/moonlight/tunnel?{query}"), "Upgrade: kvm-box-tunnel\r\nConnection: Upgrade\r\n")
    }
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn status_line(resp: &[u8]) -> u16 {
        assert!(resp.len() >= 12, "short response: {:?}", String::from_utf8_lossy(resp));
        std::str::from_utf8(&resp[9..12]).unwrap().parse().unwrap()
    }
    fn local_status(d: &Decision) -> u16 {
        match d {
            Decision::Local(r) => status_line(r),
            Decision::Proxy(_) => 0,
        }
    }
    fn decide_str(raw: &str) -> Decision {
        decide(raw.as_bytes(), b"", &digest())
    }
    fn proxied(d: Decision) -> Plan {
        match d {
            Decision::Proxy(p) => p,
            Decision::Local(r) => panic!("refused: {}", String::from_utf8_lossy(&r)),
        }
    }
    fn wait_until(f: impl Fn() -> bool) -> bool {
        (0..300).any(|_| f() || {
            thread::sleep(Duration::from_millis(10));
            false
        })
    }

    // ---------------------------------------------------------- unit: secrets

    #[test]
    fn credential_loader_requires_released_password() {
        assert_eq!(load_owner_digest(None, false), Err(CredentialError::Missing));
        assert_eq!(load_owner_digest(Some("".into()), false), Err(CredentialError::Empty));
        assert_eq!(load_owner_digest(Some("a".repeat(1025).into()), false), Err(CredentialError::TooLong));
        // The limit counts UTF-8 bytes, not characters.
        let exact = "é".repeat(512);
        assert_eq!(load_owner_digest(Some(exact.clone().into()), false), Ok(sha256(exact.as_bytes())));
        assert_eq!(load_owner_digest(Some("é".repeat(513).into()), false), Err(CredentialError::TooLong));
        assert_eq!(load_owner_digest(Some(PASSWORD.into()), false), Ok(digest()));
    }

    #[test]
    fn legacy_hash_is_refused_even_with_password() {
        assert_eq!(load_owner_digest(Some(PASSWORD.into()), true), Err(CredentialError::LegacyHash));
        assert_eq!(load_owner_digest(None, true), Err(CredentialError::LegacyHash));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_password_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(load_owner_digest(Some(OsString::from_vec(vec![0x66, 0xff, 0x6f])), false), Err(CredentialError::NotUtf8));
    }

    #[test]
    fn basic_owner_authorization() {
        let d = digest();
        let good = STANDARD.encode(format!("owner:{PASSWORD}"));
        assert!(owner_authorized(format!("Basic {good}").as_bytes(), &d));
        let colon = sha256(b"pa:ss");
        assert!(owner_authorized(format!("Basic {}", STANDARD.encode("owner:pa:ss")).as_bytes(), &colon));
        for bad in [
            format!("Basic {}", STANDARD.encode(format!("admin:{PASSWORD}"))),
            format!("Basic {}", STANDARD.encode("owner:wrong")),
            format!("Basic {}", STANDARD.encode("owner:")),
            format!("Basic {}", STANDARD.encode(PASSWORD)),
            format!("Basic {}", STANDARD.encode(format!("owner:{PASSWORD}x"))),
            format!("Bearer {good}"),
            format!("Basic  {good}"),
            format!("basic{good}"),
            "Basic !!!".to_string(),
        ] {
            assert!(!owner_authorized(bad.as_bytes(), &d), "{bad}");
        }
    }

    #[test]
    fn sha1_and_websocket_accept_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(websocket_accept(b"dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    // ---------------------------------------------------------- unit: policy

    #[test]
    fn every_route_requires_exactly_one_owner_authorization() {
        let wrong = format!("Authorization: Basic {}\r\n", STANDARD.encode("owner:synthetic-wrong"));
        for target in ["/", "/api/status", "/desktop/vnc.html", "/desktop/websockify", "/nope"] {
            for raw in [
                format!("GET {target} HTTP/1.1\r\nHost: {HOST}\r\n\r\n"),
                format!("GET {target} HTTP/1.1\r\nHost: {HOST}\r\n{}{}\r\n", auth(), auth()),
                format!("GET {target} HTTP/1.1\r\nHost: {HOST}\r\n{}{wrong}\r\n", auth()),
                format!("GET {target} HTTP/1.1\r\nHost: {HOST}\r\n{wrong}\r\n"),
            ] {
                match decide_str(&raw) {
                    Decision::Local(r) => {
                        assert_eq!(status_line(&r), 401, "{raw}");
                        assert!(String::from_utf8_lossy(&r).contains("WWW-Authenticate: Basic realm=\"KVM Box\""));
                    }
                    Decision::Proxy(_) => panic!("unauthenticated request proxied: {raw}"),
                }
            }
        }
        assert_eq!(local_status(&decide_str(&request("GET", "/nope", ""))), 404);
    }

    #[test]
    fn refuses_smuggling_and_malformed_requests() {
        let a = auth();
        let cases: Vec<(String, u16)> = vec![
            (request("POST", "/desktop/compute/x", "Transfer-Encoding: chunked\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: 5\r\nTransfer-Encoding: identity\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: 5\r\nContent-Length: 5\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: +5\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: 0x5\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: 5, 5\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: -1\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: 99999999999999999999999\r\n"), 400),
            (request("POST", "/desktop/compute/x", "Content-Length: 8388609\r\n"), 413),
            (request("GET", "/desktop/compute/x", "Content-Length: 1\r\n"), 400),
            (request("GET", "http://127.0.0.1:19080/v1/start", ""), 400),
            (request("GET", "//evil.example/x", ""), 400),
            (request("GET", "*", ""), 400),
            (request("GET", "/desktop/../api/status", ""), 400),
            (request("GET", "/desktop/%2e%2e/api", ""), 400),
            (request("GET", "/desktop/a%2fb", ""), 400),
            (request("GET", "/desktop/v%31/start", ""), 400),
            (request("GET", "/desktop/a%zz", ""), 400),
            (request("GET", "/desktop/a%00", ""), 400),
            (request("GET", "/desktop/a\\b", ""), 400),
            (request("GET", "/desktop//x", ""), 400),
            (request("GET", "/desktop/./x", ""), 400),
            (request("GET", "/desktop/x#frag", ""), 400),
            (request("PUT", "/desktop/x", ""), 405),
            (request("DELETE", "/", ""), 405),
            (request("POST", "/", ""), 405),
            (request("GET", "/api/start", ""), 405),
            (request("POST", "/api/status", "X-KVM-Box-Request: 1\r\n"), 405),
            (request("POST", "/api/start", ""), 403),
            (request("POST", "/api/start", "X-KVM-Box-Request: 0\r\n"), 403),
            (request("POST", "/api/start", "X-KVM-Box-Request: 1\r\nX-KVM-Box-Request: 1\r\n"), 400),
            (request("POST", "/api/stop", "X-KVM-Box-Request: 1\r\nContent-Length: 2\r\n"), 400),
            (request("GET", "/api/status?x=1", ""), 400),
            (request("GET", "/desktop/x", "Origin: https://evil.example\r\n"), 403),
            (request("GET", "/desktop/x", "Origin: null\r\n"), 403),
            (request("GET", "/desktop/x", &format!("Origin: ftp://{HOST}\r\n")), 403),
            (request("POST", "/api/start", "X-KVM-Box-Request: 1\r\nOrigin: https://evil.example\r\n"), 403),
            (request("GET", "/desktop/x", "Upgrade: websocket\r\nConnection: Upgrade\r\n"), 400),
            (request("GET", "/desktop/x", "Upgrade: h2c\r\nConnection: Upgrade, HTTP2-Settings\r\n"), 400),
            (request("GET", "/desktop/websockify", "Connection: Upgrade\r\n"), 400),
            (request("GET", "/desktop/websockify", "Upgrade: websocket\r\n"), 400),
            (request("GET", "/desktop/websockify", ""), 400),
            (request("GET", "/desktop/v1/start", ""), 404),
            (request("GET", "/desktop/v1", ""), 404),
            (request("GET", "/desktop", ""), 404),
            (request("GET", "/desktopx/y", ""), 404),
            (request("GET", "/v1/status", ""), 404),
            (request("GET", "/desktop/x", "Expect: something\r\n"), 417),
            (request("GET", "/desktop/x", &"X-A: 1\r\n".repeat(70)), 431),
            (request("GET", "/desktop/x", "X-Bad: a\x01b\r\n"), 400),
            (format!("GET / HTTP/1.1\r\nHost: {HOST}\r\nHost: other\r\n{a}\r\n"), 400),
            (format!("GET / HTTP/1.1\r\n{a}\r\n"), 400),
            (format!("GET / HTTP/1.1\r\nHost: bad host\r\n{a}\r\n"), 400),
            (format!("GET / HTTP/1.0\r\nHost: {HOST}\r\n{a}\r\n"), 505),
            (format!("GET / HTTP/1.1\r\nHost: {HOST}\n{a}\r\n"), 400),
        ];
        for (raw, want) in cases {
            assert_eq!(local_status(&decide_str(&raw)), want, "{raw:?}");
        }
    }

    #[test]
    fn refuses_pipelined_bytes_beyond_declared_body() {
        let d = digest();
        assert_eq!(local_status(&decide(request("GET", "/desktop/x", "").as_bytes(), b"GET /api/start HTTP/1.1\r\n\r\n", &d)), 400);
        assert_eq!(local_status(&decide(request("GET", "/", "").as_bytes(), b"x", &d)), 400);
        let post = request("POST", "/desktop/compute/x", "Content-Length: 4\r\n");
        assert_eq!(local_status(&decide(post.as_bytes(), b"abcdGET", &d)), 400);
        let plan = proxied(decide(post.as_bytes(), b"abcd", &d));
        assert_eq!(plan.prefix, b"abcd");
        assert_eq!(plan.body_len, 4);
        assert_eq!(local_status(&decide(ws_request().as_bytes(), b"\x81\x00", &d)), 400);
    }

    #[test]
    fn forwarded_head_strips_owner_credentials_and_identity_headers() {
        let raw = format!(
            "POST /desktop/compute/v1/chat?stream=1 HTTP/1.1\r\nHost: {HOST}\r\n{}Proxy-Authorization: Basic Zm9vOmJhcg==\r\nForwarded: for=1.2.3.4\r\nX-Forwarded-For: 1.2.3.4\r\nX-Forwarded-Host: evil.example\r\nX-Real-IP: 1.2.3.4\r\nX-Peer-Uid: 0\r\nX-Remote-User: owner\r\nCookie: a=b\r\nOrigin: https://{HOST}\r\nKeep-Alive: 5\r\nConnection: keep-alive\r\nX-Compute-Authorization: Bearer synthetic-compute-token\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
            auth()
        );
        let plan = proxied(decide(raw.as_bytes(), b"{}", &digest()));
        let head = String::from_utf8(plan.head.clone()).unwrap();
        assert!(head.starts_with("POST /compute/v1/chat?stream=1 HTTP/1.1\r\nHost: 127.0.0.1:19080\r\n"), "{head}");
        assert!(head.contains("X-Compute-Authorization: Bearer synthetic-compute-token\r\n"));
        assert!(head.contains("Content-Type: application/json\r\n"));
        assert!(head.ends_with("Content-Length: 2\r\nConnection: close\r\n\r\n"));
        let lower = head.to_ascii_lowercase();
        for banned in ["\r\nauthorization:", "proxy-authorization", "forwarded", "x-real-ip", "x-peer-uid", "x-remote-user", "cookie", "origin", "keep-alive", "evil.example", "1.2.3.4", "desk.example"] {
            assert!(!lower.contains(banned), "{banned} leaked: {head}");
        }
        assert!(!head.contains(PASSWORD));
        assert!(!head.contains(&STANDARD.encode(format!("owner:{PASSWORD}"))));
        assert!(!lower.contains(&hex(&digest())));
        assert_eq!(plan.prefix, b"{}");

        let api = proxied(decide_str(&request("POST", "/api/start", "X-KVM-Box-Request: 1\r\nX-Forwarded-For: 1.2.3.4\r\nX-Compute-Authorization: t\r\n")));
        assert_eq!(api.head, b"POST /v1/start HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nAccept: application/json\r\nX-KVM-Box-Request: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let status = proxied(decide_str(&request("GET", "/api/status", "")));
        assert_eq!(status.head, b"GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    }

    #[test]
    fn websocket_upgrade_plan_and_refusals() {
        let plan = proxied(decide_str(&ws_request()));
        let head = String::from_utf8(plan.head.clone()).unwrap();
        assert!(head.starts_with("GET /websockify HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: binary\r\n"), "{head}");
        assert!(!head.to_ascii_lowercase().contains("origin") && !head.contains("Authorization"));
        match plan.kind {
            Kind::WebSocket { accept, protocols } => {
                assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
                assert_eq!(protocols, vec![b"binary".to_vec()]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(local_status(&decide_str(&ws_with("8", "dGhlIHNhbXBsZSBub25jZQ=="))), 400);
        assert_eq!(local_status(&decide_str(&ws_with("13", "c2hvcnQ="))), 400);
        assert_eq!(local_status(&decide_str(&ws_with("13", "not base64!"))), 400);
        assert_eq!(local_status(&decide_str(&ws_request().replacen("GET", "POST", 1))), 405);
        let dup = ws_request().replacen("Upgrade: websocket\r\n", "Upgrade: websocket\r\nSec-WebSocket-Key: AAAAAAAAAAAAAAAAAAAAAA==\r\n", 1);
        assert_eq!(local_status(&decide_str(&dup)), 400);
    }

    #[test]
    fn moonlight_upgrade_plan_and_refusals() {
        for q in ["port=47984&transport=tcp", "transport=udp&port=48000", "port=48010&transport=tcp"] {
            let plan = proxied(decide_str(&ml_request(q)));
            assert!(String::from_utf8_lossy(&plan.head).starts_with(&format!("GET /api/moonlight/tunnel?{q} HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nUpgrade: kvm-box-tunnel\r\nConnection: Upgrade\r\n")));
            assert!(matches!(plan.kind, Kind::Moonlight));
        }
        for q in ["port=47990&transport=tcp", "port=47998&transport=tcp", "port=47984&transport=udp", "port=47984&port=47989&transport=tcp", "port=047984&transport=tcp", "port=%34%37984&transport=tcp", "port=47984&transport=tcp&host=10.0.0.1", ""] {
            assert_eq!(local_status(&decide_str(&ml_request(q))), 400, "{q}");
        }
        assert_eq!(local_status(&decide_str(&request("GET", "/desktop/api/moonlight/tunnel", "Upgrade: kvm-box-tunnel\r\nConnection: Upgrade\r\n"))), 400);
        assert_eq!(local_status(&decide_str(&request("GET", "/desktop/api/moonlight/tunnel?port=47984&transport=tcp", "Upgrade: websocket\r\nConnection: Upgrade\r\n"))), 400);
        assert_eq!(local_status(&decide_str(&ml_request("port=47984&transport=tcp").replacen("GET", "POST", 1))), 405);
        let plan = proxied(decide(ml_request("port=47984&transport=tcp").as_bytes(), b"EARLY", &digest()));
        assert_eq!(plan.early, b"EARLY");
        assert!(plan.prefix.is_empty());
    }

    #[test]
    fn upstream_response_validation() {
        let ws = Kind::WebSocket { accept: "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".into(), protocols: vec![b"binary".to_vec()] };
        let ok = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-WebSocket-Protocol: binary\r\n\r\nRFB";
        assert!(matches!(classify_response(ok.as_bytes(), &ws, true), Ok(RespStep::Switch)));
        for bad in [
            ok.replace("s3pP", "AAAA"),
            ok.replace("Connection: Upgrade\r\n", ""),
            ok.replace("Protocol: binary", "Protocol: chat"),
            ok.replace("\r\n\r\n", "\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"),
            ok.replace("Upgrade: websocket", "Upgrade: kvm-box-tunnel"),
        ] {
            assert!(classify_response(bad.as_bytes(), &ws, true).is_err(), "{bad}");
        }
        assert!(classify_response(ok.as_bytes(), &Kind::Plain, true).is_err());
        assert!(matches!(classify_response(ML_101, &Kind::Moonlight, true), Ok(RespStep::Switch)));
        assert!(classify_response(ok.as_bytes(), &Kind::Moonlight, true).is_err());
        assert!(matches!(classify_response(b"HTTP/1.1 200 OK\r\nContent-", &Kind::Plain, true), Ok(RespStep::More)));
        let huge = format!("HTTP/1.1 200 OK\r\nX: {}", "a".repeat(MAX_HEAD + 10));
        assert!(classify_response(huge.as_bytes(), &Kind::Plain, true).is_err());
        assert!(matches!(classify_response(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n\r\n", &Kind::Plain, true), Ok(RespStep::Interim(25))));
        assert!(classify_response(b"HTTP/1.1 2OO OK\r\n\r\n", &Kind::Plain, true).is_err());

        let resp = b"HTTP/1.1 418 Teapot\r\nConnection: keep-alive, X-Hop\r\nKeep-Alive: 5\r\nX-Hop: 1\r\nContent-Length: 2\r\nLocation: /vnc.html\r\n\r\nhi";
        match classify_response(resp, &Kind::Plain, true) {
            Ok(RespStep::Final(head, n)) => {
                assert_eq!(String::from_utf8(head).unwrap(), "HTTP/1.1 418 Teapot\r\nContent-Length: 2\r\nLocation: /desktop/vnc.html\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: SAMEORIGIN\r\nContent-Security-Policy: frame-ancestors 'self'\r\nConnection: close\r\n\r\n");
                assert_eq!(n, resp.len() - 2);
            }
            _ => panic!("expected final"),
        }
        for (loc, desktop, kept) in [("https://evil.example/x", true, false), ("//evil.example/x", true, false), ("vnc.html", true, true), ("/x", false, false)] {
            let r = format!("HTTP/1.1 302 Found\r\nLocation: {loc}\r\n\r\n");
            let Ok(RespStep::Final(head, _)) = classify_response(r.as_bytes(), &Kind::Plain, desktop) else { panic!() };
            assert_eq!(String::from_utf8_lossy(&head).contains("Location"), kept, "{loc}");
        }
        // An upstream refusal of an upgrade is an ordinary response with its status preserved.
        assert!(matches!(classify_response(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n", &ws, true), Ok(RespStep::Final(h, _)) if h.starts_with(b"HTTP/1.1 403 ")));
    }

    struct Trickle {
        accepted: Vec<u8>,
        budget: usize,
    }
    impl Write for Trickle {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.budget == 0 {
                self.budget = 3;
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.budget -= 1;
            let n = b.len().min(1000);
            self.accepted.extend_from_slice(&b[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn bounded_buffer_preserves_order_across_partial_writes() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut src = io::Cursor::new(data.clone());
        let mut buf = Buf::new();
        let mut sink = Trickle { accepted: Vec::new(), budget: 3 };
        loop {
            let r = buf.fill_from(&mut src, usize::MAX);
            assert!(buf.len() <= BUF_CAP);
            buf.drain_to(&mut sink);
            if matches!(r, Io::Eof) && buf.is_empty() {
                break;
            }
        }
        assert_eq!(sink.accepted, data);
        let mut full = Buf::new();
        assert!(full.push(&vec![0; BUF_CAP]));
        assert!(!full.push(b"x"));
        assert!(matches!(full.fill_from(&mut io::Cursor::new(vec![1u8; 10]), usize::MAX), Io::Blocked));
        let mut limited = Buf::new();
        assert!(matches!(limited.fill_from(&mut io::Cursor::new(vec![7u8; 100]), 10), Io::Moved(10)));
    }

    // ------------------------------------------------------ integration harness

    struct TestServer {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
    fn server(upstream: SocketAddr) -> TestServer {
        server_with(upstream, Duration::from_secs(2), Duration::from_secs(5))
    }
    fn server_with(upstream: SocketAddr, header_timeout: Duration, idle_timeout: Duration) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let cfg = Config { upstream, owner_digest: digest(), header_timeout, idle_timeout };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || serve(&listener, &cfg, &flag));
        TestServer { addr, stop, thread: Some(thread) }
    }

    struct Mock {
        addr: SocketAddr,
        accepts: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }
    fn mock<F: Fn(TcpStream) + Send + Sync + 'static>(handler: F) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        let (a, s) = (accepts.clone(), stop.clone());
        thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        a.fetch_add(1, Ordering::SeqCst);
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                        let h = handler.clone();
                        thread::spawn(move || h(stream));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Mock { addr, accepts, stop }
    }

    fn connect(addr: SocketAddr) -> TcpStream {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    }
    fn read_head(s: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && head.len() < 70_000 {
            match s.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        head
    }
    fn header_value(head: &[u8], name: &str) -> Option<String> {
        String::from_utf8_lossy(head).split("\r\n").skip(1).find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
    fn read_all(s: &mut TcpStream) -> Vec<u8> {
        let mut v = Vec::new();
        let _ = s.read_to_end(&mut v);
        v
    }
    fn exchange(addr: SocketAddr, raw: &[u8]) -> Vec<u8> {
        let mut s = connect(addr);
        s.write_all(raw).unwrap();
        read_all(&mut s)
    }
    fn split_response(resp: &[u8]) -> (String, Vec<u8>) {
        let p = find_head_end(resp).expect("response head");
        (String::from_utf8_lossy(&resp[..p]).into_owned(), resp[p..].to_vec())
    }
    fn echo(mut s: TcpStream) {
        let mut b = [0u8; 8192];
        loop {
            match s.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if s.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = s.shutdown(Shutdown::Both);
    }
    fn flood_data() -> Vec<u8> {
        (0..FLOOD).map(|i| (i % 241) as u8).collect()
    }

    #[derive(Clone)]
    struct Seen {
        head: String,
        body: Vec<u8>,
        extra: usize,
    }
    type Log = Arc<Mutex<Vec<Seen>>>;

    /// Reads exactly Content-Length, then proves no further request bytes arrive.
    fn http_echo(log: Log) -> impl Fn(TcpStream) + Send + Sync + 'static {
        move |mut s: TcpStream| {
            let head = read_head(&mut s);
            let len = header_value(&head, "content-length").map_or(0, |v| v.parse().unwrap());
            let mut body = vec![0; len];
            s.read_exact(&mut body).unwrap();
            s.set_read_timeout(Some(Duration::from_millis(250))).unwrap();
            let mut probe = [0u8; 64];
            let extra = s.read(&mut probe).unwrap_or(0);
            log.lock().unwrap().push(Seen { head: String::from_utf8_lossy(&head).into_owned(), body: body.clone(), extra });
            let mut out = format!("HTTP/1.1 201 Created\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
            out.extend_from_slice(&body);
            let _ = s.write_all(&out);
        }
    }

    /// Companion stand-in dispatching on the forwarded request target.
    fn companion(log: Arc<Mutex<Vec<String>>>) -> impl Fn(TcpStream) + Send + Sync + 'static {
        move |mut s: TcpStream| {
            let head = read_head(&mut s);
            let text = String::from_utf8_lossy(&head).into_owned();
            log.lock().unwrap().push(text.clone());
            let target = text.split(' ').nth(1).unwrap_or("").to_string();
            match target.as_str() {
                "/slow" => {
                    thread::sleep(Duration::from_millis(1500));
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nslow");
                }
                t if t.starts_with("/websockify") => {
                    let key = header_value(&head, "sec-websocket-key").unwrap_or_default();
                    let reply = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: binary\r\n\r\nRFB 003.008\n", websocket_accept(key.as_bytes()));
                    if s.write_all(reply.as_bytes()).is_ok() {
                        echo(s);
                    }
                }
                "/api/moonlight/tunnel?port=47984&transport=tcp" => {
                    if s.write_all(ML_101).is_ok() {
                        echo(s);
                    }
                }
                "/api/moonlight/tunnel?port=47989&transport=tcp" => {
                    if s.write_all(ML_101).is_ok() {
                        let _ = s.write_all(&flood_data());
                    }
                }
                _ => {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfast");
                }
            }
        }
    }
    fn open_ws(addr: SocketAddr) -> TcpStream {
        let mut s = connect(addr);
        s.write_all(ws_request().as_bytes()).unwrap();
        let h = read_head(&mut s);
        assert!(h.starts_with(b"HTTP/1.1 101 "), "{}", String::from_utf8_lossy(&h));
        let mut greet = [0u8; 12];
        s.read_exact(&mut greet).unwrap();
        assert_eq!(&greet, b"RFB 003.008\n");
        s
    }
    fn open_ml(addr: SocketAddr, query: &str) -> TcpStream {
        let mut s = connect(addr);
        s.write_all(ml_request(query).as_bytes()).unwrap();
        let h = read_head(&mut s);
        assert!(h.starts_with(b"HTTP/1.1 101 "), "{}", String::from_utf8_lossy(&h));
        s
    }

    // --------------------------------------------------------- integration

    #[test]
    fn dashboard_is_local_and_hardened() {
        let up = mock(|_s| {});
        let srv = server(up.addr);
        let (head, body) = split_response(&exchange(srv.addr, request("GET", "/", "").as_bytes()));
        assert!(head.starts_with("HTTP/1.1 200 "));
        for h in ["Cache-Control: no-store", "X-Content-Type-Options: nosniff", "frame-ancestors 'none'"] {
            assert!(head.contains(h), "{h}");
        }
        assert!(String::from_utf8_lossy(&body).contains("path=desktop/websockify"));
        assert_eq!(up.accepts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refused_requests_never_dial_companion() {
        let up = mock(|_s| {});
        let srv = server(up.addr);
        let wrong = format!("Authorization: Basic {}\r\n", STANDARD.encode("owner:synthetic-wrong"));
        for raw in [
            format!("GET / HTTP/1.1\r\nHost: {HOST}\r\n\r\n"),
            format!("GET /api/status HTTP/1.1\r\nHost: {HOST}\r\n{wrong}\r\n"),
            format!("POST /api/start HTTP/1.1\r\nHost: {HOST}\r\nX-KVM-Box-Request: 1\r\n{}{wrong}\r\n", auth()),
            format!("GET /desktop/websockify HTTP/1.1\r\nHost: {HOST}\r\n{}{}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n", auth(), auth()),
            format!("GET /desktop/api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\nHost: {HOST}\r\nUpgrade: kvm-box-tunnel\r\nConnection: Upgrade\r\n\r\n"),
            format!("POST /desktop/compute/v1/chat HTTP/1.1\r\nHost: {HOST}\r\nX-Compute-Authorization: Bearer synthetic-compute-token\r\nContent-Length: 2\r\n\r\n{{}}"),
        ] {
            let resp = exchange(srv.addr, raw.as_bytes());
            assert_eq!(status_line(&resp), 401, "{raw}");
            assert!(String::from_utf8_lossy(&resp).contains("WWW-Authenticate: Basic realm=\"KVM Box\""));
        }
        for raw in [
            request("POST", "/api/start", ""),
            request("POST", "/desktop/x", "Transfer-Encoding: chunked\r\n"),
            request("GET", "/desktop/x", "Origin: https://evil.example\r\n"),
            ml_request("port=47990&transport=tcp"),
            request("POST", "/desktop/x", "Content-Length: 8388609\r\n"),
            request("GET", "/desktop/x", "Upgrade: websocket\r\nConnection: Upgrade\r\n"),
        ] {
            assert!(status_line(&exchange(srv.addr, raw.as_bytes())) >= 400, "{raw}");
        }
        let big = request("GET", "/desktop/x", &format!("X-Pad: {}\r\n", "a".repeat(MAX_HEAD)));
        assert_eq!(status_line(&exchange(srv.addr, big.as_bytes())), 431);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(up.accepts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn streams_exact_body_and_strips_credentials_end_to_end() {
        let log = Log::default();
        let up = mock(http_echo(log.clone()));
        let srv = server(up.addr);
        let body: Vec<u8> = (0..(1usize << 20) + 123).map(|i| (i * 31 % 256) as u8).collect();
        let mut s = connect(srv.addr);
        let head = request(
            "POST",
            "/desktop/compute/v1/echo",
            &format!("Content-Length: {}\r\nX-Compute-Authorization: Bearer synthetic-compute-token\r\nX-Forwarded-For: 203.0.113.9\r\nForwarded: for=203.0.113.9\r\nProxy-Authorization: Basic c3ludGg6eA==\r\nOrigin: https://{HOST}\r\n", body.len()),
        );
        s.write_all(head.as_bytes()).unwrap();
        for (i, chunk) in body.chunks(7919).enumerate() {
            s.write_all(chunk).unwrap();
            if i % 37 == 0 {
                thread::sleep(Duration::from_millis(3));
            }
        }
        // Bytes after the declared body must never be forwarded as another request.
        s.write_all(b"GET /desktop/smuggled HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let (rhead, rbody) = split_response(&read_all(&mut s));
        assert!(rhead.starts_with("HTTP/1.1 201 Created\r\n"), "{rhead}");
        assert!(rhead.contains("Connection: close"));
        assert_eq!(rbody, body);
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].body, body);
        assert_eq!(seen[0].extra, 0);
        let h = &seen[0].head;
        assert!(h.starts_with("POST /compute/v1/echo HTTP/1.1\r\nHost: 127.0.0.1:19080\r\n"), "{h}");
        assert!(h.contains("X-Compute-Authorization: Bearer synthetic-compute-token\r\n"));
        let lower = h.to_ascii_lowercase();
        for banned in ["\r\nauthorization:", "proxy-authorization", "forwarded", "203.0.113.9", "origin", "desk.example"] {
            assert!(!lower.contains(banned), "{banned}");
        }
        assert!(!h.contains(&STANDARD.encode(format!("owner:{PASSWORD}"))));
        assert_eq!(up.accepts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn expect_continue_is_answered_locally_then_body_streams() {
        let log = Log::default();
        let up = mock(http_echo(log.clone()));
        let srv = server(up.addr);
        let mut s = connect(srv.addr);
        s.write_all(request("POST", "/desktop/compute/v1/echo", "Content-Length: 5\r\nExpect: 100-continue\r\n").as_bytes()).unwrap();
        assert_eq!(read_head(&mut s), b"HTTP/1.1 100 Continue\r\n\r\n");
        s.write_all(b"hello").unwrap();
        let (head, body) = split_response(&read_all(&mut s));
        assert!(head.starts_with("HTTP/1.1 201 "));
        assert_eq!(body, b"hello");
        assert!(!log.lock().unwrap()[0].head.to_ascii_lowercase().contains("expect"));
    }

    #[test]
    fn truncated_body_is_never_completed_upstream() {
        let got = Arc::new(Mutex::new(None));
        let g = got.clone();
        let up = mock(move |mut s| {
            read_head(&mut s);
            let mut body = Vec::new();
            let _ = s.read_to_end(&mut body);
            *g.lock().unwrap() = Some(body.len());
        });
        let srv = server(up.addr);
        let mut c = connect(srv.addr);
        c.write_all(format!("{}abc", request("POST", "/desktop/compute/x", "Content-Length: 10\r\n")).as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(100));
        c.shutdown(Shutdown::Write).unwrap();
        assert!(read_all(&mut c).is_empty());
        assert!(wait_until(|| got.lock().unwrap().is_some()));
        assert!(got.lock().unwrap().unwrap() < 10);
    }

    #[test]
    fn api_actions_map_to_fixed_companion_routes() {
        let log = Log::default();
        let up = mock(http_echo(log.clone()));
        let srv = server(up.addr);
        assert_eq!(status_line(&exchange(srv.addr, request("GET", "/api/status", "X-Forwarded-For: 203.0.113.9\r\n").as_bytes())), 201);
        let stop = request("POST", "/api/stop", &format!("X-KVM-Box-Request: 1\r\nOrigin: http://{HOST}\r\n"));
        assert_eq!(status_line(&exchange(srv.addr, stop.as_bytes())), 201);
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen[0].head, "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
        assert_eq!(seen[1].head, "POST /v1/stop HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nAccept: application/json\r\nX-KVM-Box-Request: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }

    #[test]
    fn companion_unavailable_is_reported_locally() {
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let srv = server(dead);
        let (head, body) = split_response(&exchange(srv.addr, request("GET", "/api/status", "").as_bytes()));
        assert!(head.starts_with("HTTP/1.1 502 ") && head.contains("application/json"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].is_string());
        assert_eq!(status_line(&exchange(srv.addr, request("GET", "/desktop/vnc.html", "").as_bytes())), 502);
    }

    #[test]
    fn unsolicited_upstream_upgrade_is_refused() {
        let up = mock(|mut s| {
            read_head(&mut s);
            let _ = s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nraw-bytes");
            thread::sleep(Duration::from_millis(300));
        });
        let srv = server(up.addr);
        let resp = exchange(srv.addr, request("GET", "/desktop/vnc.html", "").as_bytes());
        assert_eq!(status_line(&resp), 502);
        assert!(!String::from_utf8_lossy(&resp).contains("raw-bytes"));
    }

    #[test]
    fn websocket_tunnel_switches_only_after_valid_101() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let up = mock(companion(log.clone()));
        let srv = server(up.addr);
        let mut s = connect(srv.addr);
        s.write_all(ws_request().as_bytes()).unwrap();
        let head = read_head(&mut s);
        assert!(head.starts_with(b"HTTP/1.1 101 "));
        assert_eq!(header_value(&head, "sec-websocket-accept").as_deref(), Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        let mut greet = [0u8; 12];
        s.read_exact(&mut greet).unwrap();
        assert_eq!(&greet, b"RFB 003.008\n");
        // A masked frame split across partial writes stays byte-exact and ordered.
        let mask = [1u8, 2, 3, 4];
        let mut frame = vec![0x82u8, 0x85, 1, 2, 3, 4];
        frame.extend(b"hello".iter().zip(mask.iter().cycle()).map(|(a, m)| a ^ m));
        for part in [&frame[..1], &frame[1..3], &frame[3..]] {
            s.write_all(part).unwrap();
            thread::sleep(Duration::from_millis(15));
        }
        let mut back = vec![0; frame.len()];
        s.read_exact(&mut back).unwrap();
        assert_eq!(back, frame);
        s.shutdown(Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        let seen = log.lock().unwrap()[0].clone();
        assert!(seen.starts_with("GET /websockify HTTP/1.1\r\nHost: 127.0.0.1:19080\r\n"), "{seen}");
        assert!(!seen.to_ascii_lowercase().contains("origin") && !seen.contains("Authorization"));
    }

    #[test]
    fn websocket_upstream_failures_never_open_tunnel() {
        let bad = mock(|mut s| {
            read_head(&mut s);
            let _ = s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: bm90LXRoZS1yaWdodC1hY2NlcHQ=\r\n\r\nRFB 003.008\n");
            thread::sleep(Duration::from_millis(300));
        });
        let srv = server(bad.addr);
        let resp = exchange(srv.addr, ws_request().as_bytes());
        assert_eq!(status_line(&resp), 502);
        assert!(!String::from_utf8_lossy(&resp).contains("RFB"));

        let refusing = mock(|mut s| {
            read_head(&mut s);
            let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: 14\r\n\r\nOrigin refused");
        });
        let srv = server(refusing.addr);
        let (head, body) = split_response(&exchange(srv.addr, ws_request().as_bytes()));
        assert!(head.starts_with("HTTP/1.1 403 ") && head.contains("Connection: close") && !head.contains("Upgrade"), "{head}");
        assert_eq!(body, b"Origin refused");
    }

    #[test]
    fn moonlight_tunnel_holds_early_bytes_until_validated_101() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let up = mock(companion(log.clone()));
        let srv = server(up.addr);
        let mut s = connect(srv.addr);
        s.write_all(format!("{}EARLY", ml_request("port=47984&transport=tcp")).as_bytes()).unwrap();
        let head = read_head(&mut s);
        assert!(head.starts_with(b"HTTP/1.1 101 "));
        assert_eq!(header_value(&head, "upgrade").as_deref(), Some("kvm-box-tunnel"));
        let mut early = [0u8; 5];
        s.read_exact(&mut early).unwrap();
        assert_eq!(&early, b"EARLY");
        // More than one buffer's worth, written in odd pieces and echoed concurrently.
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let mut reader = s.try_clone().unwrap();
        let expected = payload.clone();
        let rt = thread::spawn(move || {
            let mut got = vec![0; expected.len()];
            reader.read_exact(&mut got).unwrap();
            got == expected
        });
        for c in payload.chunks(5003) {
            s.write_all(c).unwrap();
        }
        assert!(rt.join().unwrap());
        s.shutdown(Shutdown::Write).unwrap();
        assert!(read_all(&mut s).is_empty());
        assert!(log.lock().unwrap()[0].starts_with("GET /api/moonlight/tunnel?port=47984&transport=tcp HTTP/1.1\r\nHost: 127.0.0.1:19080\r\nUpgrade: kvm-box-tunnel\r\nConnection: Upgrade\r\n"));
    }

    #[test]
    fn moonlight_upstream_refusal_is_preserved_and_early_bytes_dropped() {
        let after = Arc::new(AtomicUsize::new(usize::MAX));
        let g = after.clone();
        let up = mock(move |mut s| {
            read_head(&mut s);
            let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 29\r\n\r\nStreaming server unavailable\n");
            s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let mut b = [0u8; 64];
            g.store(s.read(&mut b).unwrap_or(0), Ordering::SeqCst);
        });
        let srv = server(up.addr);
        let resp = exchange(srv.addr, format!("{}EARLY", ml_request("port=47984&transport=tcp")).as_bytes());
        assert_eq!(status_line(&resp), 503);
        assert!(resp.ends_with(b"Streaming server unavailable\n"));
        assert!(wait_until(|| after.load(Ordering::SeqCst) != usize::MAX));
        assert_eq!(after.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_streams_progress_independently() {
        let up = mock(companion(Arc::new(Mutex::new(Vec::new()))));
        let srv = server(up.addr);
        let addr = srv.addr;
        let slow_done = Arc::new(AtomicBool::new(false));
        let sd = slow_done.clone();
        let slow = thread::spawn(move || {
            let r = exchange(addr, request("GET", "/desktop/slow", "").as_bytes());
            sd.store(true, Ordering::SeqCst);
            r
        });
        thread::sleep(Duration::from_millis(50));
        let mut ws = open_ws(addr);
        let mut ml = open_ml(addr, "port=47984&transport=tcp");
        for i in 0..20u8 {
            ws.write_all(&[i; 100]).unwrap();
            ml.write_all(&[i ^ 0xff; 50]).unwrap();
            let mut a = [0u8; 100];
            ws.read_exact(&mut a).unwrap();
            assert!(a.iter().all(|&b| b == i));
            let mut b = [0u8; 50];
            ml.read_exact(&mut b).unwrap();
            assert!(b.iter().all(|&x| x == i ^ 0xff));
        }
        assert!(exchange(addr, request("GET", "/desktop/fast", "").as_bytes()).ends_with(b"fast"));
        assert!(!slow_done.load(Ordering::SeqCst), "other streams must progress while a request is pending");
        assert!(slow.join().unwrap().ends_with(b"slow"));
        ws.write_all(b"x").unwrap();
        let mut one = [0u8; 1];
        ws.read_exact(&mut one).unwrap();
        assert_eq!(&one, b"x");
    }

    #[test]
    fn slow_reader_is_backpressured_without_stalling_others() {
        let up = mock(companion(Arc::new(Mutex::new(Vec::new()))));
        let srv = server(up.addr);
        let mut flood = open_ml(srv.addr, "port=47989&transport=tcp");
        thread::sleep(Duration::from_millis(300));
        assert!(exchange(srv.addr, request("GET", "/desktop/fast", "").as_bytes()).ends_with(b"fast"));
        let mut ws = open_ws(srv.addr);
        ws.write_all(b"ping").unwrap();
        let mut p = [0u8; 4];
        ws.read_exact(&mut p).unwrap();
        let got = read_all(&mut flood);
        assert_eq!(got.len(), FLOOD);
        assert!(got == flood_data());
    }

    #[test]
    fn header_and_idle_deadlines_tear_down_connections() {
        let ended = Arc::new(AtomicBool::new(false));
        let e = ended.clone();
        let up = mock(move |mut s| {
            read_head(&mut s);
            if s.write_all(ML_101).is_ok() {
                echo(s);
            }
            e.store(true, Ordering::SeqCst);
        });
        let srv = server_with(up.addr, Duration::from_millis(200), Duration::from_millis(400));
        let started = Instant::now();
        let mut s = connect(srv.addr);
        s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").unwrap();
        assert_eq!(status_line(&read_all(&mut s)), 408);
        assert!(started.elapsed() < Duration::from_secs(3));
        let mut t = open_ml(srv.addr, "port=47984&transport=tcp");
        let started = Instant::now();
        let mut rest = Vec::new();
        t.read_to_end(&mut rest).unwrap();
        let idle = started.elapsed();
        assert!(idle >= Duration::from_millis(300) && idle < Duration::from_secs(3), "{idle:?}");
        assert!(wait_until(|| ended.load(Ordering::SeqCst)));
        assert_eq!(up.accepts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn client_disconnect_closes_upstream_tunnel() {
        let ended = Arc::new(AtomicBool::new(false));
        let e = ended.clone();
        let up = mock(move |mut s| {
            read_head(&mut s);
            if s.write_all(ML_101).is_ok() {
                echo(s);
            }
            e.store(true, Ordering::SeqCst);
        });
        let srv = server(up.addr);
        let mut t = open_ml(srv.addr, "port=47984&transport=tcp");
        t.write_all(b"ping").unwrap();
        let mut p = [0u8; 4];
        t.read_exact(&mut p).unwrap();
        drop(t);
        assert!(wait_until(|| ended.load(Ordering::SeqCst)));
    }

    #[test]
    fn idle_unauthenticated_slots_do_not_lock_out_owner() {
        let up = mock(|_s| {});
        let srv = server_with(up.addr, Duration::from_secs(3), Duration::from_secs(5));
        let held: Vec<TcpStream> = (0..MAX_CONNS).map(|_| connect(srv.addr)).collect();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(status_line(&exchange(srv.addr, request("GET", "/", "").as_bytes())), 200);
        assert_eq!(up.accepts.load(Ordering::SeqCst), 0);
        drop(held);
    }

    #[test]
    fn websocket_query_is_refused_and_desktop_headers_are_forced() {
        let bad = ws_request().replace("/desktop/websockify", "/desktop/websockify?token=t1");
        assert_eq!(local_status(&decide_str(&bad)), 400);
        let headers = [httparse::Header { name: "X-Content-Type-Options", value: b"bad" },
                       httparse::Header { name: "X-Frame-Options", value: b"ALLOWALL" }];
        let head = String::from_utf8(final_head(200, "OK", &headers, true)).unwrap();
        assert!(head.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(head.contains("Content-Security-Policy: frame-ancestors 'self'\r\n"));
        assert!(!head.contains("bad") && !head.contains("ALLOWALL"));
    }

    #[test]
    fn connection_limit_is_enforced_and_recovers() {
        let up = mock(companion(Arc::new(Mutex::new(Vec::new()))));
        let srv = server_with(up.addr, Duration::from_secs(3), Duration::from_secs(5));
        let held: Vec<TcpStream> = (0..MAX_CONNS).map(|_| open_ml(srv.addr, "port=47984&transport=tcp")).collect();
        thread::sleep(Duration::from_millis(200));
        let mut extra = connect(srv.addr);
        assert_eq!(status_line(&read_all(&mut extra)), 503);
        drop(held);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(status_line(&exchange(srv.addr, request("GET", "/", "").as_bytes())), 200);
    }
}
