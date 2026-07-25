//! tipline — an anonymous encrypted inbox (a SecureDrop-lite) as an Enclave
//! service app.
//!
//! A tip line is asymmetric: many senders, one recipient. The recipient
//! publishes a PUBLIC key; a source encrypts to it IN THE BROWSER against a
//! throwaway ephemeral key (ECDH P-256 → AES-256-GCM), and only the
//! recipient's private key — which never leaves the owner link's URL
//! FRAGMENT — can open anything. So the server holding ciphertext it cannot
//! read is ordinary public-key crypto.
//!
//! What is NOT ordinary, and the reason this belongs in a TEE: a source must
//! trust the PAGE they type into. Any ordinary host can serve a submission
//! page that quietly exfiltrates the plaintext or swaps the public key — the
//! crypto is irrelevant if the JavaScript is hostile. Here the page is
//! served by attested code, reproducible from this source via the on-chain
//! catalog, so a source can verify the enclave BEFORE typing a word. That is
//! the property SecureDrop-style tools need and web apps normally can't give.
//!
//! What the server never learns: the plaintext, the keys, who submitted a
//! tip. What it does see: opaque blobs, their sizes, their arrival times, and
//! how many owner streams are open — the irreducible metadata of any inbox.
//!
//! API (bodies are opaque blobs or `k=v&` forms; JSON is emit-only):
//!   POST /api/lines       headers x-line-id, x-owner-hash, x-pubkey; body=title -> {ok}
//!   GET  /api/line?id=<id>                    -> PUBLIC {title, pubkey, tips} (count only)
//!   POST /api/tip         id=<id>&blob=<b64u> -> {ok, n} + SSE fan-out (no blob)
//!   GET  /api/inbox?id=<id>&owner=<token>     -> owner-only {title, tips[], received, kept}
//!   POST /api/tips/remove id=&owner=&n=<seq>  -> owner deletes one tip
//!   GET  /api/stream?id=<id>&owner=<token>    -> owner-verified SSE topic <id>
//!   POST /api/leave       id=<id>&s=<tag>     -> close a tagged stream (beacon)
//!   GET  /api/stats                           -> counts only, never ids
//!   GET  / , /t/<id> , /i/<id>   UI     GET /ping   liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, url_decode, Request, Response, Server};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BLOB: usize = 64 * 1024; // base64url chars per tip (~48 KiB ciphertext)
const MAX_BODY: usize = 96 * 1024;
const MAX_LINES: usize = 1_000;
const MAX_TOTAL_BYTES: usize = 48 * 1024 * 1024; // stored tip blobs, all lines
const RING_CAP: usize = 200; // tips held per line; older ones fall off
const LINE_TTL: u64 = 30 * 24 * 3600; // a line dies this long after its last tip
const MAX_TITLE_CHARS: usize = 80;

struct Tip {
    n: u64,
    ts: u64,
    blob: String, // opaque base64url ciphertext; never parsed, never logged
    size: usize,
}

struct Line {
    owner_hash: String, // sha256 hex of the owner token — reads are gated on it
    pubkey: String,     // base64url raw P-256 point; senders encrypt against it
    title: String,      // shown on the public submission page
    tips: VecDeque<Tip>,
    seq: u64,           // numbers tips; survives ring eviction so n never repeats
    received: u64,      // tips ever accepted on this line
    #[allow(dead_code)] // bookkeeping only; never exposed
    created: u64,
    last_activity: u64,
}

struct App {
    lines: HashMap<String, Line>,
    total_bytes: usize,   // stored tip-blob bytes across all lines
    opened: u64,          // lifetime lines opened
    lifetime_tips: u64,   // lifetime tips accepted, across live and dead lines
    expired: u64,         // lines swept after LINE_TTL idle
}

impl App {
    fn held(&self) -> usize {
        self.lines.values().map(|l| l.tips.len()).sum()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("tipline/0.1.0", 8080);
    let mut app = App {
        lines: HashMap::new(),
        total_bytes: 0,
        opened: 0,
        lifetime_tips: 0,
        expired: 0,
    };
    let mut last_sweep = 0u64;
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            route(&mut app, &mut srv, key, &req);
        }
        let t = now();
        if t.saturating_sub(last_sweep) >= 60 {
            last_sweep = t;
            let before = app.lines.len();
            app.lines.retain(|_, l| l.last_activity + LINE_TTL >= t);
            let swept = before - app.lines.len();
            if swept > 0 {
                app.expired += swept as u64;
                app.total_bytes = app
                    .lines
                    .values()
                    .flat_map(|l| l.tips.iter())
                    .map(|tip| tip.blob.len())
                    .sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.opened + app.lifetime_tips, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[tipline] holding {} lines / {} tips ({} KiB); lifetime lines={} tips={} expired={}",
                    app.lines.len(),
                    app.held(),
                    app.total_bytes / 1024,
                    app.opened,
                    app.lifetime_tips,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for /, /t/<id> (submit), /i/<id> (inbox); the
        // client routes on pathname, so a hard reload must Just Work.
        ("GET", p)
            if p == "/" || p == "/index.html" || p.starts_with("/t/") || p.starts_with("/i/") =>
        {
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
        ("GET", "/ping") => Response::new(200, "OK").body("text/plain", "ok\n"),
        ("GET", "/api/stats") => json(
            200,
            "OK",
            format!(
                "{{\"lines\":{},\"tips\":{},\"lifetime_tips\":{}}}",
                app.lines.len(),
                app.held(),
                app.lifetime_tips
            ),
        ),
        ("POST", "/api/lines") => api_lines(app, req),
        ("GET", "/api/line") => api_line(app, req),
        ("POST", "/api/tip") => api_tip(app, srv, req),
        ("GET", "/api/inbox") => api_inbox(app, req),
        ("POST", "/api/tips/remove") => api_remove(app, req),
        ("POST", "/api/leave") => api_leave(app, srv, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Line ids are client-generated: ^[a-z0-9]{10,16}$. The server never
/// generates randomness and never picks a name.
fn valid_id(id: &str) -> bool {
    (10..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// A base64url token (blob, pubkey): [A-Za-z0-9_-], no padding.
fn is_b64u(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Stream tags are whatever opaque random string the browser minted:
/// never stored, never logged, compared only to close the right stream.
fn valid_tag(t: &str) -> bool {
    (8..=64).contains(&t.len()) && is_b64u(t)
}

fn api_lines(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-line-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let owner_hash = req.header("x-owner-hash").unwrap_or("");
    if owner_hash.len() != 64 || !owner_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return json(400, "Bad Request", "{\"error\":\"bad owner hash\"}".into());
    }
    let owner_hash = owner_hash.to_ascii_lowercase();
    let pubkey = req.header("x-pubkey").unwrap_or("");
    if !(80..=140).contains(&pubkey.len()) || !is_b64u(pubkey) {
        return json(400, "Bad Request", "{\"error\":\"bad pubkey\"}".into());
    }
    let pubkey = pubkey.to_string();
    // The body is the percent-encoded title; encodeURIComponent output round-
    // trips exactly through url_decode (it never emits a bare '+').
    let raw = String::from_utf8_lossy(&req.body);
    let title = url_decode(raw.trim()).unwrap_or_default();
    let title = title.trim().to_string();
    let n = title.chars().count();
    if n == 0 || n > MAX_TITLE_CHARS || title.chars().any(|c| (c as u32) < 0x20) {
        return json(400, "Bad Request", "{\"error\":\"bad title\"}".into());
    }
    if app.lines.len() >= MAX_LINES {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.lines.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    app.opened += 1;
    app.lines.insert(
        id,
        Line {
            owner_hash,
            pubkey,
            title,
            tips: VecDeque::new(),
            seq: 0,
            received: 0,
            created: t,
            last_activity: t,
        },
    );
    json(200, "OK", "{\"ok\":true}".into())
}

/// PUBLIC submission info — title, the pubkey to encrypt against, and a live
/// tip COUNT (a source may want to see the line is alive). Never the tips.
fn api_line(app: &App, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    if !valid_id(&id) {
        return json(404, "Not Found", "{\"error\":\"gone\"}".into());
    }
    match app.lines.get(&id) {
        Some(l) => json(
            200,
            "OK",
            format!(
                "{{\"title\":\"{}\",\"pubkey\":\"{}\",\"tips\":{}}}",
                httpd::json_escape(&l.title),
                l.pubkey,
                l.tips.len()
            ),
        ),
        None => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    }
}

/// Look up a live line by id; misses of every kind (bad id, unknown,
/// expired-but-unswept) are ONE indistinguishable 404.
fn live_line(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) {
        return Err(gone());
    }
    match app.lines.get(id) {
        Some(l) if l.last_activity + LINE_TTL >= now() => Ok(id.to_string()),
        Some(_) => {
            let l = app.lines.remove(id).unwrap();
            app.total_bytes -= l.tips.iter().map(|tip| tip.blob.len()).sum::<usize>();
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

/// Accept one sealed tip and fan a NOTIFICATION (never the ciphertext) out to
/// the owner's live streams — the page fetches the blob on demand, so a
/// topic-guesser learns nothing from the SSE path.
fn api_tip(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_line(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let blob = form_get(&body, "blob").unwrap_or_default();
    if blob.is_empty() || blob.len() > MAX_BLOB || !is_b64u(&blob) {
        return json(400, "Bad Request", "{\"error\":\"bad blob\"}".into());
    }
    let line = app.lines.get_mut(&id).unwrap();
    // A full ring frees its oldest slot, so only NET growth counts toward the
    // byte cap; at capacity we say so rather than dropping someone's tip.
    let freed = if line.tips.len() >= RING_CAP {
        line.tips.front().map(|tip| tip.blob.len()).unwrap_or(0)
    } else {
        0
    };
    if app.total_bytes + blob.len() > MAX_TOTAL_BYTES + freed {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if line.tips.len() >= RING_CAP {
        let old = line.tips.pop_front().unwrap();
        app.total_bytes -= old.size;
    }
    line.seq += 1;
    let n = line.seq;
    let ts = now();
    line.last_activity = ts;
    line.received += 1;
    let size = blob.len();
    app.total_bytes += size;
    app.lifetime_tips += 1;
    // The event carries NO blob — just enough for the owner's page to know a
    // tip landed and refetch. Ciphertext stays off the notification path.
    let event = format!("event: tip\ndata: {{\"n\":{n},\"ts\":{ts},\"size\":{size}}}");
    line.tips.push_back(Tip { n, ts, blob, size });
    srv.broadcast(&id, &event);
    json(200, "OK", format!("{{\"ok\":true,\"n\":{n}}}"))
}

/// Prove ownership by presenting the token whose SHA-256 is the stored
/// owner_hash. A miss on the line is a 404; a wrong token on a real line is a
/// 403 — the two are distinct because the id here is already the owner's.
fn owned_line<'a>(app: &'a App, id: &str, token: &str) -> Result<&'a Line, Response> {
    if !valid_id(id) {
        return Err(json(404, "Not Found", "{\"error\":\"gone\"}".into()));
    }
    match app.lines.get(id) {
        Some(l) if sha256::hex(token.as_bytes()) == l.owner_hash => Ok(l),
        Some(_) => Err(json(403, "Forbidden", "{\"error\":\"denied\"}".into())),
        None => Err(json(404, "Not Found", "{\"error\":\"gone\"}".into())),
    }
}

/// Owner-only: the tips themselves (sealed blobs), newest first, for
/// in-browser decryption. Gated on the owner token's hash.
fn api_inbox(app: &mut App, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    let token = form_get(&req.query, "owner").unwrap_or_default();
    let line = match owned_line(app, &id, &token) {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let tips = line
        .tips
        .iter()
        .rev() // newest first
        .map(|tip| {
            format!(
                "{{\"n\":{},\"ts\":{},\"blob\":\"{}\"}}",
                tip.n, tip.ts, tip.blob
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    json(
        200,
        "OK",
        format!(
            "{{\"title\":\"{}\",\"tips\":[{}],\"received\":{},\"kept\":{}}}",
            httpd::json_escape(&line.title),
            tips,
            line.received,
            line.tips.len()
        ),
    )
}

/// Owner deletes one tip by its sequence number (after reading/exporting it).
fn api_remove(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = form_get(&body, "id").unwrap_or_default();
    let token = form_get(&body, "owner").unwrap_or_default();
    // Verify ownership before touching state (owned_line borrows immutably).
    if let Err(resp) = owned_line(app, &id, &token) {
        return resp;
    }
    let n: u64 = match form_get(&body, "n").and_then(|v| v.parse().ok()) {
        Some(n) => n,
        None => return json(404, "Not Found", "{\"error\":\"no such tip\"}".into()),
    };
    let line = app.lines.get_mut(&id).unwrap();
    if let Some(pos) = line.tips.iter().position(|tip| tip.n == n) {
        let tip = line.tips.remove(pos).unwrap();
        app.total_bytes -= tip.size;
        json(200, "OK", "{\"ok\":true}".into())
    } else {
        json(404, "Not Found", "{\"error\":\"no such tip\"}".into())
    }
}

/// Owner-verified SSE on the line's topic: only the holder of the private
/// key's companion owner token gets the live "a tip landed" pulse.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let id = form_get(&req.query, "id").unwrap_or_default();
    let token = form_get(&req.query, "owner").unwrap_or_default();
    match owned_line(app, &id, &token) {
        Ok(_) => {
            // No initial frame — the page fetches the inbox first, then
            // attaches here for the increments.
            srv.upgrade_sse(key, &id, "");
            // An opaque per-tab tag lets a leave beacon name this exact stream
            // later — a proxy hop may hold the socket open after the tab dies.
            if let Some(tag) = form_get(&req.query, "s").filter(|t| valid_tag(t)) {
                srv.tag_sse(key, &tag);
            }
        }
        Err(resp) => srv.respond(key, resp),
    }
}

/// `sendBeacon` on pagehide: close the named stream now instead of waiting
/// for a proxy to notice the tab died. Fire-and-forget by design — the answer
/// is 200 whatever happened, so a beacon can't probe line ids.
fn api_leave(_app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = form_get(&body, "id").unwrap_or_default();
    let tag = form_get(&body, "s").unwrap_or_default();
    if valid_id(&id) && valid_tag(&tag) {
        srv.drop_sse(&id, &tag);
    }
    json(200, "OK", "{\"ok\":true}".into())
}
