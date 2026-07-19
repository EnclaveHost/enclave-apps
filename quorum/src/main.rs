//! quorum — M-of-N approval release for secrets (two-person rule /
//! break-glass escrow), as an Enclave service app.
//!
//! The server is an approval counter for OPAQUE ciphertext: the browser
//! encrypts with AES-256-GCM (WebCrypto), the key rides the recipient's
//! link URL FRAGMENT (never sent to any server), and this process stores
//! only `{client-chosen id -> ciphertext blob, threshold M, N approver
//! hashes, one approval bit per seat}` in memory. Until M of the N
//! designated approvers have presented their tokens NOBODY gets the blob —
//! not the recipient holding the link, not the operator, not any single
//! approver. A two-person rule is only as strong as the impossibility of
//! the second signature being skipped, and whoever runs an ordinary server
//! can always skip it: read the disk, flip the flag, forge the ledger.
//! Here the code that counts approvals is attested and reproducible from
//! this source via the on-chain catalog — the operator can't overrule the
//! count any more than a stranger can. Once the quorum is met it is met
//! for good (approvals become final); if it is never met by the armed
//! deadline, the only copy is erased unrevealed.
//!
//! What the server never learns: the plaintext, the key, who created a
//! drop, who is waiting on it. What it never discloses: WHICH approvers
//! signed — peek and take carry only the count. What it refuses to say:
//! whether an id never existed, expired unmet, was burned, or was read
//! out — every miss is the same 404.
//!
//! API (bodies are opaque blobs or `k=v&` forms; JSON is emit-only):
//!   POST /api/arm              headers x-drop-id, x-threshold, x-approver-hashes,
//!                              x-reads?, x-ttl?, x-window?, x-burn-hash?; body = blob
//!   POST /api/peek             id=<id>                 -> {threshold, approvers, approvals, released, …}
//!   POST /api/approve          id=<id>&token=<secret>  -> count one approval (idempotent)
//!   POST /api/revoke-approval  id=<id>&token=<secret>  -> withdraw it — pre-release only
//!   POST /api/take             id=<id>                 -> 423 until quorum; then {blob, reads_left}
//!   POST /api/burn             id=<id>&token=<secret>  -> creator revoke (sha256 must match), any state
//!   GET  /api/stats                                    -> counts only, never ids
//!   GET  /            UI        GET /ping     liveness

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
const MAX_APPROVERS: usize = 12;
const MIN_TTL: u64 = 3600; // armed lifetime: the quorum's deadline
const MAX_TTL: u64 = 30 * 24 * 3600;
const DEFAULT_TTL: u64 = 7 * 24 * 3600;
const MIN_WINDOW: u64 = 3600; // post-release availability
const MAX_WINDOW: u64 = 7 * 24 * 3600;
const DEFAULT_WINDOW: u64 = 7 * 24 * 3600;
const MAX_READS: u32 = 25;

struct Entry {
    blob: String,
    threshold: u32,               // M — approvals that release the blob
    approver_hashes: Vec<String>, // N sha256 hexes; the index is an approver's seat
    approved: Vec<bool>,          // one bit per seat — the whole ledger
    released_at: Option<u64>,     // set the moment the count first reaches M
    window: u64,                  // post-release availability
    reads_left: u32,
    burn_hash: Option<String>,    // creator's revoke, dead-drop style
    expires_at: u64,              // armed deadline: unmet quorums erase unrevealed
}

/// The count the whole app turns on — and the only thing it ever tells
/// anyone about the ledger. Which seats are true stays in this process.
fn approvals(e: &Entry) -> u32 {
    e.approved.iter().filter(|&&b| b).count() as u32
}

fn released(e: &Entry) -> bool {
    e.released_at.is_some()
}

struct App {
    drops: HashMap<String, Entry>,
    total_bytes: usize,
    armed: u64,     // lifetime counters, for the stats line
    released: u64,  // quorums met
    delivered: u64, // fully read out
    burned: u64,
    expired: u64,   // erased by the sweep — unmet deadline or lapsed window
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("quorum/0.1.0", 8080);
    let mut app = App {
        drops: HashMap::new(),
        total_bytes: 0,
        armed: 0,
        released: 0,
        delivered: 0,
        burned: 0,
        expired: 0,
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
            let mut lapsed = 0u64;
            app.drops.retain(|_, e| {
                // Released drops live out their window from the moment the
                // quorum was met; never-released drops live to the armed
                // deadline, then the only copy goes — unrevealed.
                let alive = match e.released_at {
                    Some(r) => r + e.window > t,
                    None => e.expires_at > t,
                };
                if !alive {
                    lapsed += 1;
                }
                alive
            });
            if lapsed > 0 {
                app.expired += lapsed;
                app.total_bytes = app.drops.values().map(|e| e.blob.len()).sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.armed, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[quorum] holding {} sealed drops ({} KiB); lifetime armed={} released={} delivered={} burned={} expired={}",
                    app.drops.len(),
                    app.total_bytes / 1024,
                    app.armed,
                    app.released,
                    app.delivered,
                    app.burned,
                    app.expired
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
                "{{\"armed\":{},\"bytes\":{},\"lifetime_armed\":{},\"released\":{},\"delivered\":{},\"burned\":{},\"expired\":{}}}",
                app.drops.len(),
                app.total_bytes,
                app.armed,
                app.released,
                app.delivered,
                app.burned,
                app.expired
            ),
        ),
        ("POST", "/api/arm") => api_arm(app, req),
        ("POST", "/api/peek") => api_peek(app, req),
        ("POST", "/api/approve") => api_approve(app, req),
        ("POST", "/api/revoke-approval") => api_revoke_approval(app, req),
        ("POST", "/api/take") => api_take(app, req),
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

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

fn api_arm(app: &mut App, req: &Request) -> Response {
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
    // The quorum definition: M (threshold) of the N approver hashes. The
    // client hashes each approver token before it sends anything; the
    // server never sees a token until it is presented for counting.
    let approver_hashes: Vec<String> = req
        .header("x-approver-hashes")
        .unwrap_or("")
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .collect();
    if approver_hashes.is_empty()
        || approver_hashes.len() > MAX_APPROVERS
        || !approver_hashes.iter().all(|h| valid_hash(h))
    {
        return json(400, "Bad Request", "{\"error\":\"bad approver hashes\"}".into());
    }
    let threshold: u32 = match req.header("x-threshold").and_then(|v| v.parse().ok()) {
        // M = 0 would release with no signatures; M > N could never release.
        Some(m) if m >= 1 && m as usize <= approver_hashes.len() => m,
        _ => return json(400, "Bad Request", "{\"error\":\"bad threshold\"}".into()),
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
    let window: u64 = match req.header("x-window") {
        None => DEFAULT_WINDOW,
        Some(v) => match v.parse() {
            Ok(w) if (MIN_WINDOW..=MAX_WINDOW).contains(&w) => w,
            _ => return json(400, "Bad Request", "{\"error\":\"bad window\"}".into()),
        },
    };
    let burn_hash = match req.header("x-burn-hash") {
        Some(h) if valid_hash(h) => Some(h.to_ascii_lowercase()),
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
    let seats = approver_hashes.len();
    app.total_bytes += blob.len();
    app.armed += 1;
    app.drops.insert(
        id,
        Entry {
            blob,
            threshold,
            approver_hashes,
            approved: vec![false; seats],
            released_at: None,
            window,
            reads_left: reads,
            burn_hash,
            expires_at,
        },
    );
    json(200, "OK", format!("{{\"ok\":true,\"expires_at\":{expires_at}}}"))
}

/// Look up a live drop by the request body's `id=`; misses of every kind
/// (bad id, unknown, deadline unmet, window lapsed) are ONE
/// indistinguishable 404.
fn live_id(app: &mut App, body: &str) -> Result<String, Response> {
    let id = form_get(body, "id").unwrap_or_default();
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(&id) {
        return Err(gone());
    }
    let t = now();
    let alive = |e: &Entry| match e.released_at {
        Some(r) => r + e.window > t,
        None => e.expires_at > t,
    };
    match app.drops.get(&id) {
        Some(e) if alive(e) => Ok(id),
        Some(_) => {
            let e = app.drops.remove(&id).unwrap();
            app.total_bytes -= e.blob.len();
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

/// Find the presenting approver's seat, or answer 403. The token was never
/// stored — only its hash was, at arm time, and only its seat's bit moves.
fn seat_of(e: &Entry, token: &str) -> Result<usize, Response> {
    if !token.is_empty() {
        let hash = sha256::hex(token.as_bytes());
        if let Some(i) = e.approver_hashes.iter().position(|h| *h == hash) {
            return Ok(i);
        }
    }
    Err(json(403, "Forbidden", "{\"error\":\"bad token\"}".into()))
}

fn api_peek(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let t = now();
    let e = &app.drops[&id];
    // The count, never the ledger: nothing here says WHICH seats approved,
    // so holding the recipient link (or one approver token) teaches you
    // only how close the quorum is.
    let window_left = match e.released_at {
        Some(r) => (r + e.window).saturating_sub(t).to_string(),
        None => "null".into(),
    };
    json(
        200,
        "OK",
        format!(
            "{{\"threshold\":{},\"approvers\":{},\"approvals\":{},\"released\":{},\"reads_left\":{},\"size\":{},\"expires_in\":{},\"window_left\":{}}}",
            e.threshold,
            e.approver_hashes.len(),
            approvals(e),
            released(e),
            e.reads_left,
            e.blob.len(),
            e.expires_at.saturating_sub(t),
            window_left
        ),
    )
}

fn api_approve(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let token = form_get(&body, "token").unwrap_or_default();
    let e = app.drops.get_mut(&id).unwrap();
    let seat = match seat_of(e, &token) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    // Approving twice is one approval — the ledger is per-seat bits, not a
    // tally, so no approver can be counted as two people.
    e.approved[seat] = true;
    let n = approvals(e);
    if e.released_at.is_none() && n >= e.threshold {
        // The quorum is met — one-way, from here the window runs. Approvals
        // landing after this are recorded but change nothing.
        e.released_at = Some(now());
        app.released += 1;
    }
    json(
        200,
        "OK",
        format!(
            "{{\"ok\":true,\"approvals\":{},\"threshold\":{},\"released\":{}}}",
            n,
            e.threshold,
            released(e)
        ),
    )
}

fn api_revoke_approval(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let token = form_get(&body, "token").unwrap_or_default();
    let e = app.drops.get_mut(&id).unwrap();
    let seat = match seat_of(e, &token) {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    if released(e) {
        // A met quorum is final: letting signatures walk back out after the
        // blob became servable would make "released" a lie in hindsight.
        return json(409, "Conflict", "{\"error\":\"released\"}".into());
    }
    e.approved[seat] = false;
    json(
        200,
        "OK",
        format!("{{\"ok\":true,\"approvals\":{}}}", approvals(e)),
    )
}

fn api_take(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let e = app.drops.get_mut(&id).unwrap();
    if !released(e) {
        // 423, not 404: the drop exists and anyone with the link may say so —
        // the secret itself stays sealed until the quorum, not the server, says.
        return json(
            423,
            "Locked",
            format!(
                "{{\"error\":\"sealed\",\"approvals\":{},\"threshold\":{}}}",
                approvals(e),
                e.threshold
            ),
        );
    }
    e.reads_left -= 1;
    let reads_left = e.reads_left;
    let blob = if reads_left == 0 {
        let e = app.drops.remove(&id).unwrap();
        app.total_bytes -= e.blob.len();
        app.delivered += 1;
        e.blob
    } else {
        e.blob.clone()
    };
    json(
        200,
        "OK",
        format!("{{\"blob\":\"{blob}\",\"reads_left\":{reads_left}}}"),
    )
}

fn api_burn(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let token = form_get(&body, "token").unwrap_or_default();
    let e = &app.drops[&id];
    let Some(want) = &e.burn_hash else {
        return json(403, "Forbidden", "{\"error\":\"not burnable\"}".into());
    };
    if token.is_empty() || sha256::hex(token.as_bytes()) != *want {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    // Burn works in ANY state — sealed or released, the creator's revoke
    // erases whatever reads remain. Erasure is the one thing no quorum gates.
    let e = app.drops.remove(&id).unwrap();
    app.total_bytes -= e.blob.len();
    app.burned += 1;
    json(200, "OK", "{\"ok\":true}".into())
}
