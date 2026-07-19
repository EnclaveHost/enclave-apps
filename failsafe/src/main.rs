//! failsafe — a dead-man's switch and time capsule for secrets, as an
//! Enclave service app.
//!
//! The server is a time lock for OPAQUE ciphertext: the browser encrypts
//! with AES-256-GCM (WebCrypto), the key rides the recipients' link URL
//! FRAGMENT (never sent to any server), and this process stores only
//! `{client-chosen id -> ciphertext blob, release moment, reads left}` in
//! memory. Until that moment NOBODY gets the blob — not the recipients
//! holding the link, not the operator, not even the sender (they still hold
//! the plaintext; the service provably won't serve the blob early). A
//! capsule's moment is fixed at arm time; a switch's moment is a rolling
//! deadline every hash-verified check-in pushes out by the interval, and a
//! disarm erases. Ordinary servers can promise all this; their operators
//! can also read the disk and lie about the clock. Here the gate is
//! attested code holding RAM-only ciphertext, reproducible from this
//! source via the on-chain catalog. (Release timing trusts the platform
//! clock — the same trust billing already requires.)
//!
//! What the server never learns: the plaintext, the key, who armed a drop,
//! who is waiting on it. What it refuses to say: whether an id never
//! existed, was disarmed, was read out, or lapsed — every miss is the same
//! 404.
//!
//! API (bodies are opaque blobs or `k=v&` forms; JSON is emit-only):
//!   POST /api/arm     headers x-drop-id, x-mode, x-release-in|x-interval,
//!                     x-window?, x-reads?, x-owner-hash; body = blob
//!   POST /api/peek    id=<id>                 -> {mode, released, releases_in, …}
//!   POST /api/checkin id=<id>&token=<secret>  -> push a switch's deadline out
//!   POST /api/disarm  id=<id>&token=<secret>  -> erase, in any state
//!   POST /api/take    id=<id>                 -> 423 while sealed; then {blob, reads_left}
//!   GET  /api/stats                           -> counts only, never ids
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
const MIN_RELEASE_IN: u64 = 5; // capsule: keeps the smoke test honest, harms nobody
const MAX_RELEASE_IN: u64 = 30 * 24 * 3600;
const MIN_INTERVAL: u64 = 300; // switch: a check-in cadence, not a stopwatch
const MAX_INTERVAL: u64 = 30 * 24 * 3600;
const MIN_WINDOW: u64 = 3600;
const MAX_WINDOW: u64 = 7 * 24 * 3600;
const DEFAULT_WINDOW: u64 = 7 * 24 * 3600;
const MAX_READS: u32 = 25;

enum Mode {
    Capsule,
    Switch,
}

impl Mode {
    fn as_str(&self) -> &'static str {
        match self {
            Mode::Capsule => "capsule",
            Mode::Switch => "switch",
        }
    }
}

struct Entry {
    blob: String,
    mode: Mode,
    release_at: u64,          // capsule: fixed; switch: last check-in + interval
    interval: u64,            // switch only; 0 for a capsule
    window: u64,              // post-release availability
    reads_left: u32,
    owner_hash: String,       // sha256 hex — both the check-in and the disarm credential
    released_at: Option<u64>, // set the first time anything observes now >= release_at
}

/// The one question the whole app turns on.
fn released(e: &Entry, t: u64) -> bool {
    t >= e.release_at
}

struct App {
    drops: HashMap<String, Entry>,
    total_bytes: usize,
    armed: u64,     // lifetime counters, for the stats line
    released: u64,  // crossed the release moment
    delivered: u64, // fully read out
    disarmed: u64,
    expired: u64,   // post-release window lapsed with reads remaining
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("failsafe/0.1.0", 8080);
    let mut app = App {
        drops: HashMap::new(),
        total_bytes: 0,
        armed: 0,
        released: 0,
        delivered: 0,
        disarmed: 0,
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
            let mut unseen = 0u64;
            app.drops.retain(|_, e| {
                // Past the post-release window — measured from the first
                // observation of release, or from release_at itself if the
                // whole release came and went with no witness at all.
                if e.released_at.unwrap_or(e.release_at) + e.window > t {
                    return true;
                }
                lapsed += 1;
                if e.released_at.is_none() {
                    unseen += 1;
                }
                false
            });
            if lapsed > 0 {
                app.expired += lapsed;
                app.released += unseen; // they did release; nobody ever looked
                app.total_bytes = app.drops.values().map(|e| e.blob.len()).sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.armed, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[failsafe] holding {} armed drops ({} KiB); lifetime armed={} released={} delivered={} disarmed={} expired={}",
                    app.drops.len(),
                    app.total_bytes / 1024,
                    app.armed,
                    app.released,
                    app.delivered,
                    app.disarmed,
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
                "{{\"armed\":{},\"bytes\":{},\"lifetime_armed\":{},\"released\":{},\"delivered\":{},\"disarmed\":{},\"expired\":{}}}",
                app.drops.len(),
                app.total_bytes,
                app.armed,
                app.released,
                app.delivered,
                app.disarmed,
                app.expired
            ),
        ),
        ("POST", "/api/arm") => api_arm(app, req),
        ("POST", "/api/peek") => api_peek(app, req),
        ("POST", "/api/checkin") => api_checkin(app, req),
        ("POST", "/api/disarm") => api_disarm(app, req),
        ("POST", "/api/take") => api_take(app, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    }
}

fn valid_id(id: &str) -> bool {
    (16..=43).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
    // The mode decides which gate header applies: a capsule's release-in is
    // a promise, a switch's interval is a leash.
    let (mode, gate) = match req.header("x-mode") {
        Some("capsule") => match req.header("x-release-in").and_then(|v| v.parse().ok()) {
            Some(s) if (MIN_RELEASE_IN..=MAX_RELEASE_IN).contains(&s) => (Mode::Capsule, s),
            _ => return json(400, "Bad Request", "{\"error\":\"bad release-in\"}".into()),
        },
        Some("switch") => match req.header("x-interval").and_then(|v| v.parse().ok()) {
            Some(s) if (MIN_INTERVAL..=MAX_INTERVAL).contains(&s) => (Mode::Switch, s),
            _ => return json(400, "Bad Request", "{\"error\":\"bad interval\"}".into()),
        },
        _ => return json(400, "Bad Request", "{\"error\":\"bad mode\"}".into()),
    };
    let window: u64 = match req.header("x-window") {
        None => DEFAULT_WINDOW,
        Some(v) => match v.parse() {
            Ok(w) if (MIN_WINDOW..=MAX_WINDOW).contains(&w) => w,
            _ => return json(400, "Bad Request", "{\"error\":\"bad window\"}".into()),
        },
    };
    let reads: u32 = req
        .header("x-reads")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    if reads == 0 || reads > MAX_READS {
        return json(400, "Bad Request", "{\"error\":\"bad reads\"}".into());
    }
    // Unlike dead-drop's optional burn hash, the owner hash is REQUIRED:
    // an unstoppable release nobody can disarm is a bug, not a feature.
    let owner_hash = match req.header("x-owner-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return json(400, "Bad Request", "{\"error\":\"bad owner hash\"}".into()),
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
    let release_at = now() + gate;
    let interval = match mode {
        Mode::Switch => gate,
        Mode::Capsule => 0,
    };
    app.total_bytes += blob.len();
    app.armed += 1;
    app.drops.insert(
        id,
        Entry {
            blob,
            mode,
            release_at,
            interval,
            window,
            reads_left: reads,
            owner_hash,
            released_at: None,
        },
    );
    json(200, "OK", format!("{{\"ok\":true,\"release_at\":{release_at}}}"))
}

/// Look up a live drop by the request body's `id=`; misses of every kind
/// (bad id, unknown, window lapsed) are ONE indistinguishable 404.
fn live_id(app: &mut App, body: &str) -> Result<String, Response> {
    let id = form_get(body, "id").unwrap_or_default();
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(&id) {
        return Err(gone());
    }
    let t = now();
    match app.drops.get(&id) {
        Some(e) if e.released_at.unwrap_or(e.release_at) + e.window > t => Ok(id),
        Some(_) => {
            let e = app.drops.remove(&id).unwrap();
            app.total_bytes -= e.blob.len();
            app.expired += 1;
            if e.released_at.is_none() {
                app.released += 1;
            }
            Err(gone())
        }
        None => Err(gone()),
    }
}

/// The lazy release transition: the first time anything observes an entry
/// past its release moment, stamp released_at. The post-release window runs
/// from that first observation, so recipients aren't shortchanged by hours
/// nobody witnessed.
fn observe_release(app: &mut App, id: &str, t: u64) {
    if let Some(e) = app.drops.get_mut(id) {
        if e.released_at.is_none() && released(e, t) {
            e.released_at = Some(t);
            app.released += 1;
        }
    }
}

fn api_peek(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let t = now();
    observe_release(app, &id, t);
    let e = &app.drops[&id];
    // Nothing owner-ish here: no interval, no hint of the owner hash. A
    // recipient learns only what the countdown page needs.
    let window_left = if released(e, t) {
        (e.released_at.unwrap_or(e.release_at) + e.window)
            .saturating_sub(t)
            .to_string()
    } else {
        "null".into()
    };
    json(
        200,
        "OK",
        format!(
            "{{\"mode\":\"{}\",\"released\":{},\"releases_in\":{},\"reads_left\":{},\"size\":{},\"window_left\":{}}}",
            e.mode.as_str(),
            released(e, t),
            e.release_at.saturating_sub(t),
            e.reads_left,
            e.blob.len(),
            window_left
        ),
    )
}

fn api_checkin(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let t = now();
    observe_release(app, &id, t);
    let token = form_get(&body, "token").unwrap_or_default();
    let e = app.drops.get_mut(&id).unwrap();
    if token.is_empty() || sha256::hex(token.as_bytes()) != e.owner_hash {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    if matches!(e.mode, Mode::Capsule) {
        return json(409, "Conflict", "{\"error\":\"immutable\"}".into());
    }
    if released(e, t) {
        return json(409, "Conflict", "{\"error\":\"released\"}".into());
    }
    e.release_at = t + e.interval;
    json(200, "OK", format!("{{\"ok\":true,\"releases_in\":{}}}", e.interval))
}

fn api_disarm(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    observe_release(app, &id, now());
    let token = form_get(&body, "token").unwrap_or_default();
    let e = &app.drops[&id];
    if token.is_empty() || sha256::hex(token.as_bytes()) != e.owner_hash {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    // Disarm works in ANY state — even after release it erases whatever
    // reads remain. Erasure is the one thing that never comes too late.
    let e = app.drops.remove(&id).unwrap();
    app.total_bytes -= e.blob.len();
    app.disarmed += 1;
    json(200, "OK", "{\"ok\":true}".into())
}

fn api_take(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_id(app, &body) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let t = now();
    observe_release(app, &id, t);
    let e = app.drops.get_mut(&id).unwrap();
    if !released(e, t) {
        // 423, not 404: the drop exists and the recipient may say so — the
        // secret itself stays sealed until the clock, not the server, says.
        return json(
            423,
            "Locked",
            format!(
                "{{\"error\":\"sealed\",\"releases_in\":{}}}",
                e.release_at.saturating_sub(t)
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
