//! hookbin — a live webhook/request inspector as an Enclave service app.
//!
//! Point any webhook or HTTP client at the capture URL (`/b/<token>`) and
//! see exactly what it sent — method, raw target, every header, the body
//! byte for byte — streamed live to the inspector page. Public request-bin
//! services can read every payload that crosses them, and webhook payloads
//! carry real secrets: Stripe events, OAuth tokens, PII. This one runs
//! inside a hardware-attested TEE: captures live in enclave RAM only,
//! nothing is ever written to disk, and the build is reproducible from
//! this source via the on-chain catalog. The operator can't read your
//! webhooks either.
//!
//! Every axis is bounded so the process runs forever in a fixed footprint:
//! 500 bins (least-recently-active evicted), 200 captures per bin (a ring,
//! oldest dropped), 64 KiB stored per body (original length kept), 64 MiB
//! stored total, 24 h idle expiry. Logs carry counts, never tokens or
//! capture content.
//!
//! API (tokens are client-chosen, like dead-drop's ids; JSON is emit-only):
//!   POST   /api/bins                   header x-bin-id           -> create
//!   ANY    /b/<token>[/<anything>]     the capture URL -> configured reply
//!   GET    /api/bins/<token>/requests  -> JSON array, oldest first
//!   POST   /api/bins/<token>/response  headers x-status, x-ct; body = reply
//!   POST   /api/bins/<token>/clear     empty the ring, keep the bin
//!   DELETE /api/bins/<token>           remove the bin
//!   GET    /api/stream/<token>         SSE: one "event: req" per capture
//!   GET    /api/stats                  counts only, never tokens
//!   GET  / and /i/<token>  UI          GET /ping  liveness

mod httpd;

use httpd::{json, json_escape, Request, Response, Server};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 256 * 1024; // accepted on the wire; stored truncated
const MAX_STORED_BODY: usize = 64 * 1024;
const MAX_TARGET: usize = 2048;
const MAX_HEADERS: usize = 100;
const MAX_HEADER_VALUE: usize = 1024;
const MAX_CAPTURES: usize = 200; // per-bin ring, oldest dropped
const MAX_BINS: usize = 500;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024; // stored bodies, all bins
const MAX_RESP_BODY: usize = 8192;
const MAX_RESP_CT: usize = 100;
const BIN_TTL: u64 = 24 * 3600; // idle time before a bin expires

struct Capture {
    n: u64, // per-bin sequence
    ts: u64,
    method: String,
    target: String, // decoded path + raw query, as the caller sent it
    headers: Vec<(String, String)>, // as parsed: names lowercased, in order
    body: Vec<u8>,  // first MAX_STORED_BODY bytes
    truncated: bool,
    len: usize, // original body length, before truncation
}

struct Bin {
    captures: VecDeque<Capture>,
    resp_status: u16,
    resp_ct: String,
    resp_body: Vec<u8>,
    created: u64,
    last_activity: u64,
    lifetime: u64, // captures ever; survives /clear, rolled up on removal
    seq: u64,      // numbers captures; survives /clear so n never repeats
}

struct App {
    bins: HashMap<String, Bin>,
    total_bytes: usize, // stored capture-body bytes across all bins
    retired: u64,       // lifetime captures of removed bins, so stats keep counting
    expired: u64,       // bins swept after BIN_TTL idle
    evicted: u64,       // bins pushed out by the bin/byte caps
}

impl App {
    /// Captures ever accepted, across live and removed bins.
    fn lifetime(&self) -> u64 {
        self.retired + self.bins.values().map(|b| b.lifetime).sum::<u64>()
    }
    fn held(&self) -> usize {
        self.bins.values().map(|b| b.captures.len()).sum()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("hookbin/0.1.0", 8080);
    let mut app = App {
        bins: HashMap::new(),
        total_bytes: 0,
        retired: 0,
        expired: 0,
        evicted: 0,
    };
    let mut last_sweep = 0u64;
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            // SSE subscriptions keep their connection; everything else
            // gets a Response before the next poll.
            if req.method == "GET" {
                if let Some(token) = req.path.strip_prefix("/api/stream/") {
                    if valid_token(token) && app.bins.contains_key(token) {
                        // No initial frame — the client fetches the list
                        // first, then attaches here for increments.
                        srv.upgrade_sse(key, token, "");
                    } else {
                        srv.respond(key, gone());
                    }
                    continue;
                }
            }
            let (resp, event) = route(&mut app, &req);
            srv.respond(key, resp);
            if let Some((topic, ev)) = event {
                srv.broadcast(&topic, &ev);
            }
        }
        let t = now();
        if t.saturating_sub(last_sweep) >= 30 {
            last_sweep = t;
            let before = app.bins.len();
            let retired = &mut app.retired;
            app.bins.retain(|_, b| {
                let live = t.saturating_sub(b.last_activity) < BIN_TTL;
                if !live {
                    *retired += b.lifetime;
                }
                live
            });
            let swept = before - app.bins.len();
            if swept > 0 {
                app.expired += swept as u64;
                app.total_bytes = app.bins.values().map(stored_bytes).sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.lifetime(), t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[hookbin] holding {} bins / {} captures ({} KiB); lifetime captures={} bins expired={} evicted={}",
                    app.bins.len(),
                    app.held(),
                    app.total_bytes / 1024,
                    stat.0,
                    app.expired,
                    app.evicted
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, req: &Request) -> (Response, Option<(String, String)>) {
    // The capture URL first: ANY method lands there, and a hit both answers
    // the caller and fans out to live inspectors.
    if let Some(rest) = req.path.strip_prefix("/b/") {
        let token = rest.split('/').next().unwrap_or("").to_string();
        return capture(app, req, &token);
    }
    let resp = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => ui(),
        // The inspector deep link serves the same page; the client-side
        // router reads location.pathname (a hard reload must Just Work).
        ("GET", p) if p.starts_with("/i/") => ui(),
        ("GET", "/ping") => Response::new(200, "OK").body("text/plain", "ok\n"),
        ("GET", "/api/stats") => json(
            200,
            "OK",
            format!(
                "{{\"bins\":{},\"captures\":{},\"lifetime\":{}}}",
                app.bins.len(),
                app.held(),
                app.lifetime()
            ),
        ),
        ("POST", "/api/bins") => api_create(app, req),
        (_, p) if p.starts_with("/api/bins/") => api_bin(app, req, &p["/api/bins/".len()..]),
        _ => gone(),
    };
    (resp, None)
}

/// `^[a-z0-9][a-z0-9-]{7,31}$` — client-chosen, like dead-drop's ids: the
/// server never generates randomness, never picks a name.
fn valid_token(t: &str) -> bool {
    let b = t.as_bytes();
    if !(8..=32).contains(&b.len()) {
        return false;
    }
    let ok = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    ok(b[0]) && b[1..].iter().all(|&c| ok(c) || c == b'-')
}

/// Every miss — bad token, unknown, expired, deleted — is the same 404.
fn gone() -> Response {
    json(404, "Not Found", "{\"error\":\"gone\"}".into())
}

fn ui() -> Response {
    Response::new(200, "OK")
        .with("cache-control", "no-store")
        .with(
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'",
        )
        .with("referrer-policy", "no-referrer")
        .with("x-content-type-options", "nosniff")
        .body("text/html; charset=utf-8", UI)
}

fn api_create(app: &mut App, req: &Request) -> Response {
    let token = req.header("x-bin-id").unwrap_or("").to_string();
    if !valid_token(&token) {
        return json(400, "Bad Request", "{\"error\":\"bad token\"}".into());
    }
    if app.bins.contains_key(&token) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    // Full house: the least-recently-active bin makes room. A debugging
    // tool should always have space for the debugging happening right now.
    if app.bins.len() >= MAX_BINS {
        evict_oldest(app);
    }
    let t = now();
    app.bins.insert(
        token,
        Bin {
            captures: VecDeque::new(),
            resp_status: 200,
            resp_ct: "application/json".into(),
            resp_body: b"{\"ok\":true}\n".to_vec(),
            created: t,
            last_activity: t,
            lifetime: 0,
            seq: 0,
        },
    );
    json(200, "OK", "{\"ok\":true}".into())
}

/// The whole point: swallow anything, remember everything, answer with the
/// bin's configured reply, and fan the capture out to live inspectors.
fn capture(app: &mut App, req: &Request, token: &str) -> (Response, Option<(String, String)>) {
    if !valid_token(token) || !app.bins.contains_key(token) {
        return (gone(), None);
    }
    let t = now();
    // The full original target: httpd hands us the path percent-decoded
    // and the query raw; a webhook debugger must show the query string.
    let mut target = req.path.clone();
    if !req.query.is_empty() {
        target.push('?');
        target.push_str(&req.query);
    }
    let len = req.body.len();
    let bin = app.bins.get_mut(token).unwrap();
    bin.seq += 1;
    bin.lifetime += 1;
    bin.last_activity = t;
    let cap = Capture {
        n: bin.seq,
        ts: t,
        method: req.method.clone(),
        target: clip(&target, MAX_TARGET),
        headers: req
            .headers
            .iter()
            .take(MAX_HEADERS)
            .map(|(k, v)| (k.clone(), clip(v, MAX_HEADER_VALUE)))
            .collect(),
        body: req.body[..len.min(MAX_STORED_BODY)].to_vec(),
        truncated: len > MAX_STORED_BODY,
        len,
    };
    let event = format!("event: req\ndata: {}", capture_json(&cap));
    app.total_bytes += cap.body.len();
    bin.captures.push_back(cap);
    if bin.captures.len() > MAX_CAPTURES {
        if let Some(old) = bin.captures.pop_front() {
            app.total_bytes -= old.body.len();
        }
    }
    let resp = Response::new(bin.resp_status, phrase(bin.resp_status))
        .with("cache-control", "no-store")
        .body(&bin.resp_ct, bin.resp_body.clone());
    // Byte-cap pressure evicts whole least-recently-active bins. This bin
    // was active just now and one full ring sits well under the cap, so
    // the loop always ends with it intact.
    while app.total_bytes > MAX_TOTAL_BYTES && app.bins.len() > 1 {
        evict_oldest(app);
    }
    (resp, Some((token.to_string(), event)))
}

fn api_bin(app: &mut App, req: &Request, rest: &str) -> Response {
    let (token, op) = match rest.split_once('/') {
        Some((t, o)) => (t, o),
        None => (rest, ""),
    };
    if !valid_token(token) || !app.bins.contains_key(token) {
        return gone();
    }
    match (req.method.as_str(), op) {
        ("GET", "requests") => api_requests(app, token),
        ("POST", "response") => api_response(app, req, token),
        ("POST", "clear") => api_clear(app, token),
        ("DELETE", "") => api_delete(app, token),
        _ => gone(),
    }
}

fn api_requests(app: &App, token: &str) -> Response {
    let bin = &app.bins[token];
    let mut out = String::from("[");
    for (i, c) in bin.captures.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&capture_json(c));
    }
    out.push(']');
    json(200, "OK", out)
}

fn api_response(app: &mut App, req: &Request, token: &str) -> Response {
    let status: u16 = req
        .header("x-status")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if !(100..=599).contains(&status) {
        return json(400, "Bad Request", "{\"error\":\"bad status\"}".into());
    }
    let ct = req.header("x-ct").unwrap_or("");
    if ct.is_empty()
        || ct.len() > MAX_RESP_CT
        || !ct.bytes().all(|b| (0x20..=0x7e).contains(&b))
    {
        return json(400, "Bad Request", "{\"error\":\"bad content-type\"}".into());
    }
    if req.body.len() > MAX_RESP_BODY {
        return json(400, "Bad Request", "{\"error\":\"body too large\"}".into());
    }
    let bin = app.bins.get_mut(token).unwrap();
    bin.resp_status = status;
    bin.resp_ct = ct.to_string();
    bin.resp_body = req.body.clone();
    json(200, "OK", "{\"ok\":true}".into())
}

fn api_clear(app: &mut App, token: &str) -> Response {
    let bin = app.bins.get_mut(token).unwrap();
    let freed = stored_bytes(bin);
    bin.captures.clear();
    app.total_bytes -= freed;
    json(200, "OK", "{\"ok\":true}".into())
}

fn api_delete(app: &mut App, token: &str) -> Response {
    if let Some(bin) = app.bins.remove(token) {
        app.total_bytes -= stored_bytes(&bin);
        app.retired += bin.lifetime;
    }
    json(200, "OK", "{\"ok\":true}".into())
}

fn stored_bytes(b: &Bin) -> usize {
    b.captures.iter().map(|c| c.body.len()).sum()
}

/// Capacity pressure: drop the least-recently-active bin (ties broken by
/// age). Counts roll into the heartbeat; tokens are never logged.
fn evict_oldest(app: &mut App) {
    let Some(token) = app
        .bins
        .iter()
        .min_by_key(|(_, b)| (b.last_activity, b.created))
        .map(|(t, _)| t.clone())
    else {
        return;
    };
    if let Some(bin) = app.bins.remove(&token) {
        app.total_bytes -= stored_bytes(&bin);
        app.retired += bin.lifetime;
        app.evicted += 1;
    }
}

/// One capture as JSON — the /requests list items and the SSE frames use
/// this same shape, so the client renders both paths with one function.
fn capture_json(c: &Capture) -> String {
    let mut headers = String::new();
    for (i, (k, v)) in c.headers.iter().enumerate() {
        if i > 0 {
            headers.push(',');
        }
        headers.push_str("[\"");
        headers.push_str(&json_escape(k));
        headers.push_str("\",\"");
        headers.push_str(&json_escape(v));
        headers.push_str("\"]");
    }
    format!(
        "{{\"n\":{},\"ts\":{},\"method\":\"{}\",\"target\":\"{}\",\"headers\":[{}],\"body_b64\":\"{}\",\"truncated\":{},\"len\":{}}}",
        c.n,
        c.ts,
        json_escape(&c.method),
        json_escape(&c.target),
        headers,
        b64(&c.body),
        c.truncated,
        c.len
    )
}

/// Standard-alphabet base64 with padding — the one encoder this app needs.
fn b64(data: &[u8]) -> String {
    const AL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(AL[(n >> 18) as usize & 63] as char);
        out.push(AL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { AL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { AL[(n as usize) & 63] as char } else { '=' });
    }
    out
}

/// Truncate to at most `max` bytes, backing off to a char boundary.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Reason phrases for the user-configurable capture reply (any 100..=599).
fn phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        410 => "Gone",
        418 => "I'm a teapot",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        s if s < 200 => "Informational",
        s if s < 300 => "Success",
        s if s < 400 => "Redirect",
        s if s < 500 => "Client Error",
        _ => "Server Error",
    }
}
