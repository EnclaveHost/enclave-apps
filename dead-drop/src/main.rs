//! dead-drop — burn-after-reading secret sharing as an Enclave service app.
//!
//! The server is a claim-check counter for OPAQUE ciphertext: the browser
//! encrypts with AES-256-GCM (WebCrypto), the key rides the link's URL
//! FRAGMENT (never sent to any server), and this process stores only
//! `{client-chosen id -> ciphertext blob, reads left, expiry}` in memory.
//! A take is atomic by construction — wasip2 has no threads — so "exactly
//! N reveals" is a hard guarantee, and expiry/burn erase the only copy.
//! Running inside an attested TEE is what makes the claim mean something:
//! there is no disk, no operator shell, and the build is reproducible from
//! this source via the on-chain catalog.
//!
//! What the server never learns: the plaintext, the key, who created a
//! drop, who read it. What it refuses to say: whether an id never existed,
//! was consumed, or expired — every miss is the same 404.
//!
//! API (bodies are opaque blobs or `k=v&` forms; JSON is emit-only):
//!   POST /api/drop   headers x-drop-id, x-reads, x-ttl, x-burn-hash?; body = blob
//!   POST /api/take   id=<id>                 -> {blob, reads_left} (decrement/delete)
//!   POST /api/peek   id=<id>                 -> {reads_left, expires_in, size}
//!   POST /api/burn   id=<id>&token=<secret>  -> sender revoke (sha256 must match)
//!   GET  /api/stats                          -> counts only, never ids
//!   GET  /            UI        GET /ping    liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BLOB: usize = 96 * 1024; // base64url chars (~72 KiB ciphertext)
const MAX_BODY: usize = MAX_BLOB + 4 * 1024;
const MAX_DROPS: usize = 20_000;
const MAX_TOTAL_BYTES: usize = 48 * 1024 * 1024;
const MIN_TTL: u64 = 60;
const MAX_TTL: u64 = 7 * 24 * 3600;
const DEFAULT_TTL: u64 = 24 * 3600;
const MAX_READS: u32 = 25;

struct DropEntry {
    blob: String,
    reads_left: u32,
    expires_at: u64,
    burn_hash: Option<String>,
}

struct App {
    drops: HashMap<String, DropEntry>,
    total_bytes: usize,
    created: u64,  // lifetime counters, for the stats line
    consumed: u64, // fully consumed (read out)
    expired: u64,
    burned: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("dead-drop/0.1.0", 8080);
    let mut app = App {
        drops: HashMap::new(),
        total_bytes: 0,
        created: 0,
        consumed: 0,
        expired: 0,
        burned: 0,
    };
    let mut last_sweep = 0u64;
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            let resp = route(&mut app, &req);
            srv.respond(key, resp);
        }
        let t = now();
        if t.saturating_sub(last_sweep) >= 5 {
            last_sweep = t;
            let before = app.drops.len();
            app.drops.retain(|_, d| d.expires_at > t);
            let swept = before - app.drops.len();
            if swept > 0 {
                app.expired += swept as u64;
                app.total_bytes = app.drops.values().map(|d| d.blob.len()).sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.created, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[dead-drop] holding {} drops ({} KiB); lifetime created={} consumed={} expired={} burned={}",
                    app.drops.len(),
                    app.total_bytes / 1024,
                    app.created,
                    app.consumed,
                    app.expired,
                    app.burned
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, req: &Request) -> Response {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => Response::new(200, "OK")
            .with("cache-control", "no-store")
            .with(
                "content-security-policy",
                "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'",
            )
            .with("referrer-policy", "no-referrer")
            .with("x-content-type-options", "nosniff")
            .body("text/html; charset=utf-8", UI),
        ("GET", "/ping") => Response::new(200, "OK").body("text/plain", "ok\n"),
        ("GET", "/api/stats") => json(
            200,
            "OK",
            format!(
                "{{\"drops\":{},\"bytes\":{},\"created\":{},\"consumed\":{},\"expired\":{},\"burned\":{}}}",
                app.drops.len(),
                app.total_bytes,
                app.created,
                app.consumed,
                app.expired,
                app.burned
            ),
        ),
        ("POST", "/api/drop") => api_drop(app, req),
        ("POST", "/api/take") => api_take(app, req),
        ("POST", "/api/peek") => api_peek(app, req),
        ("POST", "/api/burn") => api_burn(app, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    }
}

fn valid_id(id: &str) -> bool {
    (16..=43).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn api_drop(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-drop-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let blob = match std::str::from_utf8(&req.body) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return json(400, "Bad Request", "{\"error\":\"bad blob\"}".into()),
    };
    if blob.is_empty()
        || blob.len() > MAX_BLOB
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return json(400, "Bad Request", "{\"error\":\"bad blob\"}".into());
    }
    let reads: u32 = req
        .header("x-reads")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    if reads == 0 || reads > MAX_READS {
        return json(400, "Bad Request", "{\"error\":\"bad reads\"}".into());
    }
    let ttl: u64 = req
        .header("x-ttl")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TTL);
    if !(MIN_TTL..=MAX_TTL).contains(&ttl) {
        return json(400, "Bad Request", "{\"error\":\"bad ttl\"}".into());
    }
    let burn_hash = match req.header("x-burn-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(h.to_ascii_lowercase())
        }
        Some(_) => return json(400, "Bad Request", "{\"error\":\"bad burn hash\"}".into()),
        None => None,
    };
    if app.drops.len() >= MAX_DROPS || app.total_bytes + blob.len() > MAX_TOTAL_BYTES {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.drops.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let expires_at = now() + ttl;
    app.total_bytes += blob.len();
    app.created += 1;
    app.drops.insert(
        id,
        DropEntry { blob, reads_left: reads, expires_at, burn_hash },
    );
    json(200, "OK", format!("{{\"ok\":true,\"expires_at\":{expires_at}}}"))
}

/// Look up a live drop by the request body's `id=`; misses of every kind
/// (bad id, unknown, expired) are ONE indistinguishable 404.
fn live_id(app: &mut App, body: &str) -> Result<String, Response> {
    let id = form_get(body, "id").unwrap_or_default();
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(&id) {
        return Err(gone());
    }
    match app.drops.get(&id) {
        Some(d) if d.expires_at > now() => Ok(id),
        Some(_) => {
            app.drops.remove(&id);
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

fn api_take(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let entry = app.drops.get_mut(&id).unwrap();
    entry.reads_left -= 1;
    let reads_left = entry.reads_left;
    let blob = if reads_left == 0 {
        let e = app.drops.remove(&id).unwrap();
        app.total_bytes -= e.blob.len();
        app.consumed += 1;
        e.blob
    } else {
        entry.blob.clone()
    };
    json(
        200,
        "OK",
        format!("{{\"blob\":\"{blob}\",\"reads_left\":{reads_left}}}"),
    )
}

fn api_peek(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let d = &app.drops[&id];
    json(
        200,
        "OK",
        format!(
            "{{\"reads_left\":{},\"expires_in\":{},\"size\":{}}}",
            d.reads_left,
            d.expires_at.saturating_sub(now()),
            d.blob.len()
        ),
    )
}

fn api_burn(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let token = form_get(&body, "token").unwrap_or_default();
    let d = &app.drops[&id];
    let Some(want) = &d.burn_hash else {
        return json(403, "Forbidden", "{\"error\":\"not burnable\"}".into());
    };
    if token.is_empty() || sha256::hex(token.as_bytes()) != *want {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    let e = app.drops.remove(&id).unwrap();
    app.total_bytes -= e.blob.len();
    app.burned += 1;
    json(200, "OK", "{\"ok\":true}".into())
}
