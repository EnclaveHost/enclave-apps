//! handoff — files through an attested enclave, as an Enclave service app.
//!
//! A wormhole with a warehouse in the middle: the sender's browser encrypts
//! a file chunk-by-chunk with AES-256-GCM (key in the link's URL FRAGMENT,
//! never sent), the enclave holds the ciphertext in RAM, and the receiver's
//! browser pulls and decrypts. Even the filename travels only inside an
//! encrypted manifest. The server's whole knowledge of a handoff: an opaque
//! id the client chose, a chunk count, byte totals, timestamps.
//!
//! Downloads are counted by COMPLETION, not by curiosity: a claim only
//! consumes a download when its final chunk has been served, so a dropped
//! connection doesn't burn the transfer. At zero downloads left, or TTL,
//! or the sender's burn link — the only copy is erased. Nothing ever
//! touches a disk; the platform gives service apps none.
//!
//! The wire is raw bytes both ways (application/octet-stream, no base64
//! bloat): ciphertext in via POST bodies, ciphertext out via GET bodies.
//!
//! API (headers + `k=v&` forms; JSON is emit-only):
//!   POST /api/new    headers x-drop-id, x-chunks, x-bytes, x-reads, x-ttl,
//!                    x-burn-hash?; body = encrypted manifest (<=4 KiB)
//!   POST /api/put    headers x-drop-id, x-chunk; body = raw chunk ciphertext
//!   GET  /api/meta?id=            -> {chunks, bytes, complete, reads_left,
//!                                     expires_in, manifest(b64u)}
//!   POST /api/claim  id=&t=<tok>  -> register a claim (completion counts it)
//!   GET  /api/chunk?id=&i=&t=     -> raw ciphertext bytes
//!   POST /api/burn   id=&token=   -> sender revoke (sha256 must match)
//!   GET  /api/stats               -> counts only, never ids
//!   GET  /            UI           GET /ping    liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_CHUNKS: usize = 256;
const MAX_CHUNK_BYTES: usize = 300 * 1024; // 256 KiB plaintext + GCM overhead + slack
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 192 * 1024 * 1024;
const MAX_MANIFEST: usize = 4096;
const MAX_BODY: usize = MAX_CHUNK_BYTES + 4 * 1024;
const MAX_DROPS: usize = 500;
const MAX_OPEN_CLAIMS: usize = 4; // per file
const CLAIM_TTL: u64 = 15 * 60;
const UPLOAD_STALL: u64 = 15 * 60; // incomplete uploads die this long after the last chunk
const MIN_TTL: u64 = 300;
const MAX_TTL: u64 = 72 * 3600;
const DEFAULT_TTL: u64 = 24 * 3600;
const MAX_READS: u32 = 5;

struct Claim {
    token: String,
    served: [u64; 4], // bitmap over chunk indexes
    served_count: usize,
    opened: u64,
}

struct FileDrop {
    manifest: Vec<u8>, // encrypted client-side; the name lives in here
    chunks: Vec<Option<Vec<u8>>>,
    declared_bytes: usize,
    stored_bytes: usize,
    complete: bool,
    reads_left: u32,
    claims: Vec<Claim>,
    burn_hash: Option<String>,
    expires_at: u64,
    last_put: u64,
}

struct App {
    drops: HashMap<String, FileDrop>,
    total_bytes: usize,
    created: u64, // lifetime counters, for the stats line
    delivered: u64,
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
    let mut srv = Server::bind("handoff/0.1.0", 8080);
    let mut app = App {
        drops: HashMap::new(),
        total_bytes: 0,
        created: 0,
        delivered: 0,
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
        if t.saturating_sub(last_sweep) >= 10 {
            last_sweep = t;
            let mut freed = 0usize;
            let mut lapsed = 0u64;
            app.drops.retain(|_, d| {
                // Claims age out individually; the file ages out whole —
                // expired, or stalled mid-upload with no way to finish.
                d.claims.retain(|c| t.saturating_sub(c.opened) < CLAIM_TTL);
                let dead = d.expires_at <= t
                    || (!d.complete && t.saturating_sub(d.last_put) > UPLOAD_STALL);
                if dead {
                    freed += d.stored_bytes;
                    lapsed += 1;
                }
                !dead
            });
            app.total_bytes -= freed;
            app.expired += lapsed;
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.created + app.delivered, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[handoff] holding {} files ({} MiB); lifetime sealed={} delivered={} expired={} burned={}",
                    app.drops.len(),
                    app.total_bytes / (1024 * 1024),
                    app.created,
                    app.delivered,
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
                "{{\"files\":{},\"bytes\":{},\"sealed\":{},\"delivered\":{},\"expired\":{},\"burned\":{}}}",
                app.drops.len(),
                app.total_bytes,
                app.created,
                app.delivered,
                app.expired,
                app.burned
            ),
        ),
        ("POST", "/api/new") => api_new(app, req),
        ("POST", "/api/put") => api_put(app, req),
        ("GET", "/api/meta") => api_meta(app, req),
        ("POST", "/api/claim") => api_claim(app, req),
        ("GET", "/api/chunk") => api_chunk(app, req),
        ("POST", "/api/burn") => api_burn(app, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    }
}

/// Ids and claim tokens are client-generated, dead-drop's charset exactly.
fn valid_id(id: &str) -> bool {
    (16..=43).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn gone() -> Response {
    json(404, "Not Found", "{\"error\":\"gone\"}".into())
}

fn api_new(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-drop-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let chunks: usize = match req.header("x-chunks").and_then(|v| v.parse().ok()) {
        Some(n) if (1..=MAX_CHUNKS).contains(&n) => n,
        _ => return json(400, "Bad Request", "{\"error\":\"bad chunks\"}".into()),
    };
    let declared: usize = match req.header("x-bytes").and_then(|v| v.parse().ok()) {
        Some(n) if n > 0 && n <= MAX_FILE_BYTES && n <= chunks * MAX_CHUNK_BYTES => n,
        _ => return json(400, "Bad Request", "{\"error\":\"bad bytes\"}".into()),
    };
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
    if req.body.is_empty() || req.body.len() > MAX_MANIFEST {
        return json(400, "Bad Request", "{\"error\":\"bad manifest\"}".into());
    }
    if app.drops.len() >= MAX_DROPS || app.total_bytes + declared > MAX_TOTAL_BYTES {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.drops.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    let expires_at = t + ttl;
    app.created += 1;
    app.drops.insert(
        id,
        FileDrop {
            manifest: req.body.clone(),
            chunks: vec![None; chunks],
            declared_bytes: declared,
            stored_bytes: 0,
            complete: false,
            reads_left: reads,
            claims: Vec::new(),
            burn_hash,
            expires_at,
            last_put: t,
        },
    );
    json(200, "OK", format!("{{\"ok\":true,\"expires_at\":{expires_at}}}"))
}

fn api_put(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-drop-id").unwrap_or("").to_string();
    let Some(d) = app.drops.get_mut(&id) else { return gone() };
    if d.expires_at <= now() {
        return gone();
    }
    if d.complete {
        return json(409, "Conflict", "{\"error\":\"already sealed\"}".into());
    }
    let i: usize = match req.header("x-chunk").and_then(|v| v.parse().ok()) {
        Some(n) if n < d.chunks.len() => n,
        _ => return json(400, "Bad Request", "{\"error\":\"bad chunk index\"}".into()),
    };
    if req.body.is_empty() || req.body.len() > MAX_CHUNK_BYTES {
        return json(400, "Bad Request", "{\"error\":\"bad chunk\"}".into());
    }
    let old = d.chunks[i].as_ref().map(|c| c.len()).unwrap_or(0);
    let new_total = d.stored_bytes - old + req.body.len();
    if new_total > d.declared_bytes + 1024 {
        return json(400, "Bad Request", "{\"error\":\"over declared size\"}".into());
    }
    app.total_bytes = app.total_bytes - old + req.body.len();
    d.stored_bytes = new_total;
    d.chunks[i] = Some(req.body.clone());
    d.last_put = now();
    let have = d.chunks.iter().filter(|c| c.is_some()).count();
    if have == d.chunks.len() {
        d.complete = true;
    }
    json(
        200,
        "OK",
        format!("{{\"ok\":true,\"have\":{have},\"complete\":{}}}", d.complete),
    )
}

fn api_meta(app: &mut App, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    if !valid_id(&id) {
        return gone();
    }
    let Some(d) = app.drops.get(&id) else { return gone() };
    if d.expires_at <= now() {
        return gone();
    }
    json(
        200,
        "OK",
        format!(
            "{{\"chunks\":{},\"bytes\":{},\"complete\":{},\"reads_left\":{},\"expires_in\":{},\"manifest\":\"{}\"}}",
            d.chunks.len(),
            d.declared_bytes,
            d.complete,
            d.reads_left,
            d.expires_at.saturating_sub(now()),
            b64u(&d.manifest)
        ),
    )
}

fn api_claim(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = form_get(&body, "id").unwrap_or_default();
    let token = form_get(&body, "t").unwrap_or_default();
    if !valid_id(&id) || !valid_id(&token) {
        return gone();
    }
    let t = now();
    let Some(d) = app.drops.get_mut(&id) else { return gone() };
    if d.expires_at <= t || d.reads_left == 0 {
        return gone();
    }
    if !d.complete {
        return json(409, "Conflict", "{\"error\":\"incomplete\"}".into());
    }
    d.claims.retain(|c| t.saturating_sub(c.opened) < CLAIM_TTL);
    if d.claims.len() >= MAX_OPEN_CLAIMS {
        return json(429, "Too Many Requests", "{\"error\":\"busy, try later\"}".into());
    }
    if !d.claims.iter().any(|c| c.token == token) {
        d.claims.push(Claim {
            token,
            served: [0; 4],
            served_count: 0,
            opened: t,
        });
    }
    json(200, "OK", format!("{{\"ok\":true,\"chunks\":{}}}", d.chunks.len()))
}

fn api_chunk(app: &mut App, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    let token = form_get(&req.query, "t").unwrap_or_default();
    let i: usize = match form_get(&req.query, "i").and_then(|v| v.parse().ok()) {
        Some(n) => n,
        None => return gone(),
    };
    let (body, finished, erase, freed) = {
        let Some(d) = app.drops.get_mut(&id) else { return gone() };
        if d.expires_at <= now() || !d.complete || i >= d.chunks.len() {
            return gone();
        }
        let n_chunks = d.chunks.len();
        let Some(claim) = d.claims.iter_mut().find(|c| c.token == token) else {
            return gone();
        };
        let (word, bit) = (i / 64, 1u64 << (i % 64));
        if claim.served[word] & bit == 0 {
            claim.served[word] |= bit;
            claim.served_count += 1;
        }
        let finished = claim.served_count == n_chunks;
        let body = d.chunks[i].as_ref().unwrap().clone();
        let mut erase = false;
        if finished {
            // The claim's last chunk: this download is complete and counts.
            d.claims.retain(|c| c.token != token);
            d.reads_left -= 1;
            erase = d.reads_left == 0;
        }
        (body, finished, erase, d.stored_bytes)
    };
    if finished {
        app.delivered += 1;
    }
    if erase {
        app.drops.remove(&id);
        app.total_bytes -= freed;
    }
    Response::new(200, "OK")
        .with("cache-control", "no-store")
        .body("application/octet-stream", body)
}

fn api_burn(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = form_get(&body, "id").unwrap_or_default();
    if !valid_id(&id) {
        return gone();
    }
    let Some(d) = app.drops.get(&id) else { return gone() };
    if d.expires_at <= now() {
        return gone();
    }
    let token = form_get(&body, "token").unwrap_or_default();
    let Some(want) = &d.burn_hash else {
        return json(403, "Forbidden", "{\"error\":\"not burnable\"}".into());
    };
    if token.is_empty() || sha256::hex(token.as_bytes()) != *want {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    let freed = d.stored_bytes;
    app.drops.remove(&id);
    app.total_bytes -= freed;
    app.burned += 1;
    json(200, "OK", "{\"ok\":true}".into())
}

/// base64url without padding (the manifest is the only thing we ever encode;
/// chunks travel as raw bytes in both directions).
fn b64u(data: &[u8]) -> String {
    const AB: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(AB[(n >> 18 & 63) as usize] as char);
        out.push(AB[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(AB[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(AB[(n & 63) as usize] as char);
        }
    }
    out
}
