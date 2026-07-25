//! fairdraw — provably fair drawings whose commitment precedes every entry.
//!
//! Every raffle ends with someone claiming it was rigged, and ordinarily
//! nothing can refute them: whoever holds the RNG could have rolled until
//! they liked the answer. fairdraw closes that door by construction. At
//! creation the enclave draws a secret salt and publishes sha256(salt)
//! BEFORE a single entry exists; winners are a pure function of
//! (salt, the exact entry list); at close the salt is revealed and anyone
//! can replay the draw in their browser and check the commitment. The
//! operator can't grind salts (the commitment came first), entrants can't
//! grind entries (the salt sits sealed in enclave RAM), and attestation
//! proves the code holding it is this code.
//!
//! Entrants are anonymous to the server the way ballot's voters are: an
//! entry IS the SHA-256 of a random token the browser generated, plus a
//! display name. The same token again renames the entry in place — one
//! token, one slot, position stable — so the committed list can't be
//! stuffed by re-entering. The hash of the creator's admin token is the
//! only key that closes a draw early; a deadline closes it without anyone.
//!
//! What the server never says before close: the salt. What it publishes
//! after: everything needed to recompute the result — salt, ordered entry
//! hashes, winners. Logs carry counts, never ids, names or salts.
//!
//! API (bodies are %-encoded lines or `k=v&` forms; JSON is emit-only):
//!   POST /api/draws      headers x-draw-id, x-admin-hash, x-winners,
//!                        x-ttl?, x-deadline-in?; body = %-encoded title
//!   GET  /api/draw?id=   -> state; salt/entry_hashes/winners only when closed
//!   POST /api/enter      id=&name=&token=  -> append, or rename this token's slot
//!   POST /api/close      id=&admin=        -> draw winners + reveal (idempotent)
//!   GET  /api/stream?id= -> SSE `entries` / `closed` events on topic <id>
//!   GET  /api/stats      -> counts only, never ids
//!   GET  / , /d/<id>     UI                GET /ping   liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, json_escape, url_decode, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 16 * 1024; // worst-case: an 80-char title, fully %-encoded
const MAX_DRAWS: usize = 2_000;
const MAX_ENTRIES: usize = 10_000; // per draw
const MAX_TITLE: usize = 80; // chars
const MAX_NAME: usize = 40; // chars
const MAX_WINNERS: u32 = 10;
const MIN_TTL: u64 = 3600;
const MAX_TTL: u64 = 30 * 24 * 3600;
const DEFAULT_TTL: u64 = 7 * 24 * 3600;
const MIN_DEADLINE: u64 = 60;
const CLOSED_LINGER: u64 = 7 * 24 * 3600; // closed draws stay verifiable this long

struct Draw {
    title: String,
    winners_n: u32,
    salt: [u8; 32],
    commit: String, // sha256 hex of the salt bytes, published at creation
    entries: Vec<(String, String)>, // (sha256 hex of entry token, name), insertion order
    by_token: HashMap<String, usize>, // token hash -> index into entries
    admin_hash: String, // sha256 hex of the creator's close token
    closed: bool,
    winners: Vec<usize>, // entry indices in win order, set at close
    closed_at: Option<u64>,
    deadline: Option<u64>, // auto-close time, if the creator set one
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    expires_at: u64,
}

struct App {
    draws: HashMap<String, Draw>,
    created: u64, // lifetime counters, for the stats line
    entered: u64, // accepted entries, renames included
    closed: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Hex of arbitrary bytes (sha256::hex is digest-then-hex; the revealed salt
/// needs its bytes shown as-is).
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A 32-byte salt from the entropy std already hands this target: each
/// `RandomState` is freshly seeded from the host at construction, so we run
/// a few distinct inputs through several of them, add the nanosecond clock,
/// and mix it all through SHA-256. Honest framing: the *fairness* argument
/// rests on commit-then-reveal plus attestation — the commitment is fixed
/// before any entry exists, so even a weak salt can't be ground against the
/// entry list — with host-seeded entropy underneath for unpredictability.
fn fresh_salt() -> [u8; 32] {
    use std::hash::{BuildHasher, Hasher};
    let mut mix = Vec::with_capacity(200);
    for round in 0u8..8 {
        let state = std::collections::hash_map::RandomState::new();
        for input in [&b"fairdraw-salt"[..], b"commit-first", b"reveal-after"] {
            let mut h = state.build_hasher();
            h.write(input);
            h.write(&[round]);
            mix.extend_from_slice(&h.finish().to_le_bytes());
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    mix.extend_from_slice(&nanos.to_le_bytes());
    sha256::digest(&mix)
}

/// The committed entry list, as one hash: sha256 over the concatenated
/// UTF-8 bytes of each entry's token-hash hex string, in insertion order.
fn entries_hash(entries: &[(String, String)]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(entries.len() * 64);
    for (token_hash, _) in entries {
        buf.extend_from_slice(token_hash.as_bytes());
    }
    sha256::digest(&buf)
}

/// Winner selection — recomputed byte-for-byte in the browser and by anyone
/// else, so this is the spec: seed = sha256(salt || entries_hash), then a
/// Fisher-Yates prefix over indices 0..n where round i draws
/// r = sha256(seed || i as 8 LE bytes) and swaps idx[i] with
/// idx[i + (first 8 bytes of r, LE, as u64) % (n - i)]. Winners are
/// idx[..k] in that order, k = min(winners_n, n); no entries, no winners.
fn pick_winners(salt: &[u8; 32], entries: &[(String, String)], winners_n: u32) -> Vec<usize> {
    let n = entries.len();
    let k = (winners_n as usize).min(n);
    if n == 0 {
        return Vec::new();
    }
    let mut seed_in = [0u8; 64];
    seed_in[..32].copy_from_slice(salt);
    seed_in[32..].copy_from_slice(&entries_hash(entries));
    let seed = sha256::digest(&seed_in);
    let mut idx: Vec<usize> = (0..n).collect();
    for i in 0..k {
        let mut round = [0u8; 40];
        round[..32].copy_from_slice(&seed);
        round[32..].copy_from_slice(&(i as u64).to_le_bytes());
        let r = sha256::digest(&round);
        let roll = u64::from_le_bytes(r[..8].try_into().unwrap());
        let j = i + (roll % (n - i) as u64) as usize;
        idx.swap(i, j);
    }
    idx.truncate(k);
    idx
}

fn main() {
    let mut srv = Server::bind("fairdraw/0.1.0", 8080);
    let mut app = App {
        draws: HashMap::new(),
        created: 0,
        entered: 0,
        closed: 0,
        expired: 0,
    };
    let mut last_sweep = 0u64;
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            route(&mut app, &mut srv, key, &req);
        }
        let t = now();
        if t.saturating_sub(last_sweep) >= 30 {
            last_sweep = t;
            // Deadlines first: an open draw past its auto-close time goes
            // through the same close path a manual close takes.
            let due: Vec<String> = app
                .draws
                .iter()
                .filter(|(_, d)| !d.closed && d.deadline.is_some_and(|dl| dl <= t))
                .map(|(id, _)| id.clone())
                .collect();
            for id in due {
                close_draw(&mut app, &mut srv, &id);
            }
            let mut lapsed = 0u64;
            app.draws.retain(|_, d| match d.closed_at {
                // Closed draws linger so anyone can come back and verify;
                // open draws just expire, salt never revealed.
                Some(c) => c.saturating_add(CLOSED_LINGER) > t,
                None => {
                    let keep = d.expires_at > t;
                    if !keep {
                        lapsed += 1;
                    }
                    keep
                }
            });
            app.expired += lapsed;
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.created + app.entered, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                let entries: usize = app.draws.values().map(|d| d.entries.len()).sum();
                println!(
                    "[fairdraw] holding {} draws / {} entries; lifetime draws={} entries={} closed={} expired={}",
                    app.draws.len(),
                    entries,
                    app.created,
                    app.entered,
                    app.closed,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /d/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/d/") => {
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
        ("GET", "/api/stats") => {
            let entries: usize = app.draws.values().map(|d| d.entries.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"draws\":{},\"entries\":{},\"lifetime_draws\":{},\"lifetime_entries\":{},\"closed\":{}}}",
                    app.draws.len(),
                    entries,
                    app.created,
                    app.entered,
                    app.closed
                ),
            )
        }
        ("POST", "/api/draws") => api_create(app, req),
        ("GET", "/api/draw") => api_draw(app, req),
        ("POST", "/api/enter") => api_enter(app, srv, req),
        ("POST", "/api/close") => api_close(app, srv, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Draw ids are client-generated: ^[a-z0-9]{8,16}$.
fn valid_id(id: &str) -> bool {
    (8..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn api_create(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-draw-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let admin_hash = match req.header("x-admin-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return json(400, "Bad Request", "{\"error\":\"bad admin hash\"}".into()),
    };
    let winners_n: u32 = match req.header("x-winners").and_then(|v| v.parse().ok()) {
        Some(w) if (1..=MAX_WINNERS).contains(&w) => w,
        _ => return json(400, "Bad Request", "{\"error\":\"bad winners\"}".into()),
    };
    let ttl: u64 = match req.header("x-ttl") {
        None => DEFAULT_TTL,
        Some(v) => match v.parse() {
            Ok(t) if (MIN_TTL..=MAX_TTL).contains(&t) => t,
            _ => return json(400, "Bad Request", "{\"error\":\"bad ttl\"}".into()),
        },
    };
    let deadline_in: Option<u64> = match req.header("x-deadline-in") {
        None => None,
        Some(v) => match v.parse() {
            Ok(s) if (MIN_DEADLINE..=ttl).contains(&s) => Some(s),
            _ => return json(400, "Bad Request", "{\"error\":\"bad deadline\"}".into()),
        },
    };
    // Body: the title, %-encoded so it stays one line on the wire.
    let title = match std::str::from_utf8(&req.body).ok().and_then(url_decode) {
        Some(t) => t.trim().to_string(),
        None => return json(400, "Bad Request", "{\"error\":\"bad title\"}".into()),
    };
    if title.is_empty()
        || title.chars().count() > MAX_TITLE
        || title.chars().any(|c| (c as u32) < 0x20)
    {
        return json(400, "Bad Request", "{\"error\":\"bad title\"}".into());
    }
    if app.draws.len() >= MAX_DRAWS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.draws.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    let expires_at = t + ttl;
    // The whole trick, in two lines: the salt is drawn before anyone can
    // enter, and only its hash leaves the enclave until close.
    let salt = fresh_salt();
    let commit = sha256::hex(&salt);
    app.created += 1;
    app.draws.insert(
        id,
        Draw {
            title,
            winners_n,
            salt,
            commit: commit.clone(),
            entries: Vec::new(),
            by_token: HashMap::new(),
            admin_hash,
            closed: false,
            winners: Vec::new(),
            closed_at: None,
            deadline: deadline_in.map(|s| t + s),
            created: t,
            expires_at,
        },
    );
    json(
        200,
        "OK",
        format!("{{\"ok\":true,\"commit\":\"{commit}\",\"expires_at\":{expires_at}}}"),
    )
}

/// Look up a draw a client may still address: open and unexpired, or closed
/// and inside its linger window. Every other miss is the same 404.
fn live_draw(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) {
        return Err(gone());
    }
    match app.draws.get(id) {
        Some(d) if d.closed || d.expires_at > now() => Ok(id.to_string()),
        Some(_) => {
            app.draws.remove(id);
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

/// The one draw-state shape (/api/draw, the SSE initial event, the closed
/// broadcast, the close response). The keys that decide the result —
/// `salt`, `entry_hashes`, `winners` — EXIST only once the draw is closed:
/// that asymmetry is the entire app. Names and title pass through
/// json_escape; the token hashes and salt are hex we produced ourselves.
fn draw_json(d: &Draw) -> String {
    let names = d
        .entries
        .iter()
        .map(|(_, name)| format!("\"{}\"", json_escape(name)))
        .collect::<Vec<_>>()
        .join(",");
    let closes_in = match (d.closed, d.deadline) {
        (false, Some(dl)) => dl.saturating_sub(now()).to_string(),
        _ => "null".into(),
    };
    // Open draws count down to expiry; closed ones to the end of the
    // verification window.
    let expires_in = match d.closed_at {
        Some(c) => c.saturating_add(CLOSED_LINGER).saturating_sub(now()),
        None => d.expires_at.saturating_sub(now()),
    };
    let mut out = format!(
        "{{\"title\":\"{}\",\"winners_n\":{},\"commit\":\"{}\",\"closed\":{},\"count\":{},\"names\":[{}],\"closes_in\":{},\"expires_in\":{}",
        json_escape(&d.title),
        d.winners_n,
        d.commit,
        d.closed,
        d.entries.len(),
        names,
        closes_in,
        expires_in
    );
    if d.closed {
        let winners = d
            .winners
            .iter()
            .map(|&i| format!("{{\"i\":{},\"name\":\"{}\"}}", i, json_escape(&d.entries[i].1)))
            .collect::<Vec<_>>()
            .join(",");
        let hashes = d
            .entries
            .iter()
            .map(|(h, _)| format!("\"{h}\""))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            ",\"winners\":[{}],\"salt\":\"{}\",\"entry_hashes\":[{}]",
            winners,
            to_hex(&d.salt),
            hashes
        ));
    }
    out.push('}');
    out
}

fn api_draw(app: &mut App, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_draw(app, &id) {
        Ok(id) => json(200, "OK", draw_json(&app.draws[&id])),
        Err(resp) => resp,
    }
}

fn api_enter(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_draw(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    // A deadline that passed between sweeps closes the draw right here —
    // same path as everything else — so no entry ever lands after it.
    let overdue = {
        let d = &app.draws[&id];
        !d.closed && d.deadline.is_some_and(|dl| dl <= now())
    };
    if overdue {
        close_draw(app, srv, &id);
    }
    if app.draws[&id].closed {
        return json(409, "Conflict", "{\"error\":\"closed\"}".into());
    }
    let token = form_get(&body, "token").unwrap_or_default();
    if !(16..=64).contains(&token.len()) {
        return json(400, "Bad Request", "{\"error\":\"bad token\"}".into());
    }
    let name = form_get(&body, "name").unwrap_or_default().trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return json(400, "Bad Request", "{\"error\":\"bad name\"}".into());
    }
    // The entry key is the hash of a token only the entrant's browser knows;
    // the same token again renames the slot it already holds — position
    // stable, so a re-entry can't reshuffle the committed order — and a new
    // one appends.
    let token_hash = sha256::hex(token.as_bytes());
    let d = app.draws.get_mut(&id).unwrap();
    match d.by_token.get(&token_hash) {
        Some(&i) => d.entries[i].1 = name,
        None => {
            if d.entries.len() >= MAX_ENTRIES {
                return json(507, "Insufficient Storage", "{\"error\":\"draw full\"}".into());
            }
            d.by_token.insert(token_hash.clone(), d.entries.len());
            d.entries.push((token_hash, name));
        }
    }
    let count = d.entries.len();
    app.entered += 1;
    // Everyone watching learns participation; names ride /api/draw.
    srv.broadcast(&id, &format!("event: entries\ndata: {{\"count\":{count}}}"));
    json(200, "OK", format!("{{\"ok\":true,\"count\":{count}}}"))
}

/// The one close path — /api/close, the deadline sweep, and a too-late
/// entry all land here. Idempotent: a closed draw stays exactly as it
/// closed. This is the only moment the salt leaves enclave RAM.
fn close_draw(app: &mut App, srv: &mut Server, id: &str) {
    let Some(d) = app.draws.get_mut(id) else { return };
    if d.closed {
        return;
    }
    d.winners = pick_winners(&d.salt, &d.entries, d.winners_n);
    d.closed = true;
    d.closed_at = Some(now());
    app.closed += 1;
    let payload = draw_json(&app.draws[id]);
    srv.broadcast(id, &format!("event: closed\ndata: {payload}"));
}

fn api_close(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_draw(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let admin = form_get(&body, "admin").unwrap_or_default();
    if admin.is_empty() || sha256::hex(admin.as_bytes()) != app.draws[&id].admin_hash {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    close_draw(app, srv, &id);
    json(200, "OK", draw_json(&app.draws[&id]))
}

/// SSE subscription for one draw. Late joiners get the current state as the
/// first (unnamed) event, framed exactly like /api/draw, so a reconnecting
/// client never needs a second fetch to be consistent.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_draw(app, &id) {
        Ok(id) => {
            let initial = format!("data: {}\n\n", draw_json(&app.draws[&id]));
            srv.upgrade_sse(key, &id, &initial);
        }
        Err(resp) => srv.respond(key, resp),
    }
}
