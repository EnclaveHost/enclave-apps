//! tripwire — canary tokens with a live alarm board, as an Enclave service app.
//!
//! A canary token is a URL you plant somewhere nothing should ever touch it:
//! a `passwords.xlsx` on a file share, a honeypot row in a database dump, a
//! fake AWS key in a repo, a 1×1 pixel in a document. Nobody has a legitimate
//! reason to fetch it, so a single fetch is a fact: something read a thing it
//! shouldn't have, at this second, with this user-agent, from this referer.
//!
//! The TEE angle, stated honestly: the trip log is append-only inside attested
//! code holding RAM-only state. An intruder who owns the host still cannot
//! reach in and delete the record of their own trip. They can stop the
//! deployment — but stopping it is itself visible, and a stopped board is not
//! a clean board. What they cannot do is rewrite history and leave it running
//! as if nothing happened. That's the property a log file on a VPS the
//! attacker just rooted can never offer.
//!
//! What tripwire records, and what it does not: the enclave's TLS proxy
//! terminates the connection, so no client address ever reaches this process.
//! A trip is WHAT and WHEN — method, target, user-agent, referer, timestamp —
//! never WHO. Better to say so than to imply an attribution we can't make.
//!
//! Every axis is bounded so the process runs forever in a fixed footprint:
//! 500 boards, 25 wires each, a 100-trip ring per wire (oldest dropped), 30
//! days idle before a board is swept. Logs carry counts — never a board id, a
//! canary token, or anything a trip recorded.
//!
//! API (bodies are `k=v&` forms; JSON is emit-only):
//!   POST /api/boards        headers x-board-id, x-owner-hash    -> {ok}
//!   POST /api/wires         id=&owner=&name=&kind=&token=       -> {ok}
//!   POST /api/wires/remove  id=&owner=&token=                   -> {ok}
//!   ANY  /t/<token>         THE TRIP -> an innocuous 404 + SSE fan-out
//!   GET  /api/board?id=&owner=    -> wires + trips (owner only, always)
//!   GET  /api/stream?id=&owner=   -> SSE `trip` on topic <board id>
//!   GET  /api/stats               -> counts only, never ids or tokens
//!   GET  / , /b/<id>   UI         GET /ping   liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, json_escape, Request, Response, Server};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 8 * 1024;
const MAX_BOARDS: usize = 500;
const MAX_WIRES: usize = 25; // per board
const MAX_TRIPS: usize = 100; // per-wire ring, oldest dropped
const TRIPS_SHOWN: usize = 20; // newest-first slice the owner's JSON carries
const MAX_UA: usize = 200;
const MAX_REFERER: usize = 200;
const MAX_TARGET: usize = 300;
const BOARD_TTL: u64 = 30 * 24 * 3600; // idle time before a board is swept

/// A 1×1 fully transparent GIF, 43 bytes — the smallest thing that shows
/// nothing and fetches everywhere. Hardcoded so the app generates nothing.
const PIXEL: [u8; 43] = [
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

const LINK_DECOY: &str =
    "<!doctype html><meta charset=\"utf-8\"><title>Unavailable</title>\n<p>This resource is unavailable.\n";
const DOC_DECOY: &str = "This document is unavailable.\n";

/// One fetch of a canary URL. No address field, deliberately: the proxy
/// terminates TLS and this process never sees one. What and when, not who.
struct Trip {
    n: u64, // per-wire sequence; survives the ring, so n never repeats
    ts: u64,
    ua: String,
    referer: String,
    method: String,
    target: String, // decoded path + raw query, as the tripper sent it
}

struct Wire {
    name: String,  // display text, not an identifier; json_escape on emit
    token: String, // client-chosen capability, dead-drop's id charset
    kind: String,  // link | pixel | doc — decides the decoy body, nothing else
    armed_at: u64,
    trips: VecDeque<Trip>,
    trip_count: u64,
}

struct Board {
    owner_hash: String, // sha256 hex — the only credential that reads this board
    wires: Vec<Wire>,
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    last_activity: u64, // any trip, edit, or owner read
    alarms_total: u64,
}

struct App {
    boards: HashMap<String, Board>,
    // The hot path is an intruder's fetch: canary token -> board id must be
    // one hash lookup, so this index is kept in sync on add/remove/sweep.
    token_index: HashMap<String, String>,
    alarms_total: u64, // lifetime counters, for the stats line
    lifetime_boards: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("tripwire/0.1.0", 8080);
    let mut app = App {
        boards: HashMap::new(),
        token_index: HashMap::new(),
        alarms_total: 0,
        lifetime_boards: 0,
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
            let dead: Vec<String> = app
                .boards
                .iter()
                .filter(|(_, b)| b.last_activity + BOARD_TTL < t)
                .map(|(id, _)| id.clone())
                .collect();
            for id in dead {
                let b = app.boards.remove(&id).unwrap();
                for w in &b.wires {
                    app.token_index.remove(&w.token);
                }
                app.expired += 1;
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.lifetime_boards + app.alarms_total, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[tripwire] armed {} boards / {} wires; lifetime boards={} alarms={} expired={}",
                    app.boards.len(),
                    app.token_index.len(),
                    app.lifetime_boards,
                    app.alarms_total,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    // The canary URL first: ANY method lands there — a scanner's HEAD, a
    // mail client's GET, a curl -X POST from a script that found the token.
    // All of them are trips.
    if let Some(rest) = req.path.strip_prefix("/t/") {
        let token = rest.split('/').next().unwrap_or("").to_string();
        let resp = api_trip(app, srv, req, &token);
        srv.respond(key, resp);
        return;
    }
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /b/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/b/") => {
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
            let wires: usize = app.boards.values().map(|b| b.wires.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"boards\":{},\"wires\":{},\"alarms\":{},\"lifetime_boards\":{}}}",
                    app.boards.len(),
                    wires,
                    app.alarms_total,
                    app.lifetime_boards
                ),
            )
        }
        ("POST", "/api/boards") => api_boards(app, req),
        ("POST", "/api/wires") => api_wires(app, req),
        ("POST", "/api/wires/remove") => api_wires_remove(app, req),
        ("GET", "/api/board") => api_board(app, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Board ids are client-generated: ^[a-z0-9]{10,16}$.
fn valid_board_id(id: &str) -> bool {
    (10..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Canary tokens are client-generated too, in dead-drop's id charset.
fn valid_token(token: &str) -> bool {
    (16..=43).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Wire names are display text: 1..=60 chars, anything printable —
/// json_escape carries the emit side.
fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=60).contains(&n) && !name.chars().any(|c| c.is_control())
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

/// What the tripper sees, and the whole of it. Every canary URL — live or
/// dead, of every kind — answers 404 with something dull, so probing a
/// harvested token tells an intruder nothing about whether it was armed.
/// A pixel still fires: an image fetch records the trip long before any
/// renderer cares what the status line said.
fn decoy(kind: &str) -> Response {
    let r = Response::new(404, "Not Found").with("cache-control", "no-store");
    match kind {
        "pixel" => r.body("image/gif", PIXEL.to_vec()),
        "doc" => r.body("text/plain; charset=utf-8", DOC_DECOY),
        _ => r.body("text/html; charset=utf-8", LINK_DECOY),
    }
}

/// THE trip. One hash lookup, one scan of a ≤25-wire board, one broadcast —
/// then the same nothing every other canary URL returns.
fn api_trip(app: &mut App, srv: &mut Server, req: &Request, token: &str) -> Response {
    let Some(id) = app.token_index.get(token).cloned() else {
        return decoy("link");
    };
    let t = now();
    let Some(board) = app.boards.get_mut(&id) else {
        return decoy("link");
    };
    if board.last_activity + BOARD_TTL < t {
        return decoy("link"); // expired-but-unswept; the sweeper will erase it
    }
    let Some(wire) = board.wires.iter_mut().find(|w| w.token == token) else {
        return decoy("link");
    };
    // The full original target: httpd hands us the path percent-decoded and
    // the query raw. Whatever the tripper appended is part of the evidence.
    let mut target = req.path.clone();
    if !req.query.is_empty() {
        target.push('?');
        target.push_str(&req.query);
    }
    wire.trip_count += 1;
    let trip = Trip {
        n: wire.trip_count,
        ts: t,
        ua: clip(req.header("user-agent").unwrap_or(""), MAX_UA),
        referer: clip(req.header("referer").unwrap_or(""), MAX_REFERER),
        method: req.method.clone(),
        target: clip(&target, MAX_TARGET),
    };
    // Only owner-verified subscribers are ever on this topic, so the frame
    // can name the wire by its token — an exact key the board view already
    // holds, where a display name would be ambiguous.
    let event = format!(
        "event: trip\ndata: {{\"wire\":\"{}\",\"token\":\"{}\",\"trip_count\":{},\"trip\":{}}}",
        json_escape(&wire.name),
        wire.token,
        wire.trip_count,
        trip_json(&trip)
    );
    let kind = wire.kind.clone();
    wire.trips.push_back(trip);
    if wire.trips.len() > MAX_TRIPS {
        wire.trips.pop_front();
    }
    board.last_activity = t;
    board.alarms_total += 1;
    app.alarms_total += 1;
    srv.broadcast(&id, &event);
    decoy(&kind)
}

fn trip_json(t: &Trip) -> String {
    format!(
        "{{\"n\":{},\"ts\":{},\"method\":\"{}\",\"target\":\"{}\",\"ua\":\"{}\",\"referer\":\"{}\"}}",
        t.n,
        t.ts,
        json_escape(&t.method),
        json_escape(&t.target),
        json_escape(&t.ua),
        json_escape(&t.referer)
    )
}

/// One wire as its owner sees it — the only view there is. Trips come out
/// newest first, because the one you care about is the one that just landed.
/// `trip_count` is every trip ever; `kept` is how many the ring still holds.
/// Both are emitted so the board can say plainly which older trips have aged
/// out rather than implying the log is complete.
fn wire_json(w: &Wire) -> String {
    let trips = w
        .trips
        .iter()
        .rev()
        .take(TRIPS_SHOWN)
        .map(trip_json)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"name\":\"{}\",\"kind\":\"{}\",\"token\":\"{}\",\"armed_at\":{},\"trip_count\":{},\"kept\":{},\"trips\":[{}]}}",
        json_escape(&w.name),
        w.kind,
        w.token,
        w.armed_at,
        w.trip_count,
        w.trips.len(),
        trips
    )
}

fn board_json(b: &Board) -> String {
    let wires = b.wires.iter().map(wire_json).collect::<Vec<_>>().join(",");
    format!(
        "{{\"wires\":[{}],\"alarms_total\":{}}}",
        wires, b.alarms_total
    )
}

fn api_boards(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-board-id").unwrap_or("").to_string();
    if !valid_board_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let owner_hash = match req.header("x-owner-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return json(400, "Bad Request", "{\"error\":\"bad owner hash\"}".into()),
    };
    if app.boards.len() >= MAX_BOARDS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.boards.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    app.lifetime_boards += 1;
    app.boards.insert(
        id,
        Board {
            owner_hash,
            wires: Vec::new(),
            created: t,
            last_activity: t,
            alarms_total: 0,
        },
    );
    json(200, "OK", "{\"ok\":true}".into())
}

/// Look up a live board by id; misses of every kind (bad id, unknown,
/// expired-but-unswept) are ONE indistinguishable 404.
fn live_board(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_board_id(id) {
        return Err(gone());
    }
    match app.boards.get(id) {
        Some(b) if b.last_activity + BOARD_TTL >= now() => Ok(id.to_string()),
        Some(_) => {
            let b = app.boards.remove(id).unwrap();
            for w in &b.wires {
                app.token_index.remove(&w.token);
            }
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

/// Check a presented `owner` token against the board's stored hash. This is
/// the only gate on the board — an alarm board has no public view: telling
/// the world which of your wires are quiet is telling an intruder where to
/// step.
fn verify_owner(app: &App, id: &str, token: &str) -> Result<(), Response> {
    if token.is_empty() || sha256::hex(token.as_bytes()) != app.boards[id].owner_hash {
        return Err(json(403, "Forbidden", "{\"error\":\"denied\"}".into()));
    }
    Ok(())
}

fn api_wires(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_board(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = verify_owner(app, &id, &form_get(&body, "owner").unwrap_or_default()) {
        return resp;
    }
    let name = form_get(&body, "name").unwrap_or_default();
    if !valid_name(&name) {
        return json(400, "Bad Request", "{\"error\":\"bad name\"}".into());
    }
    let kind = form_get(&body, "kind").unwrap_or_default();
    if !matches!(kind.as_str(), "link" | "pixel" | "doc") {
        return json(400, "Bad Request", "{\"error\":\"bad kind\"}".into());
    }
    let token = form_get(&body, "token").unwrap_or_default();
    if !valid_token(&token) {
        return json(400, "Bad Request", "{\"error\":\"bad token\"}".into());
    }
    if app.boards[&id].wires.len() >= MAX_WIRES {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"wire cap reached\"}".into(),
        );
    }
    // Canary tokens are global capabilities: one token, one wire, ever — the
    // O(1) trip path depends on the index staying a bijection.
    if app.token_index.contains_key(&token) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    let board = app.boards.get_mut(&id).unwrap();
    board.last_activity = t;
    board.wires.push(Wire {
        name,
        token: token.clone(),
        kind,
        armed_at: t,
        trips: VecDeque::new(),
        trip_count: 0,
    });
    app.token_index.insert(token, id);
    json(200, "OK", "{\"ok\":true}".into())
}

fn api_wires_remove(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_board(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = verify_owner(app, &id, &form_get(&body, "owner").unwrap_or_default()) {
        return resp;
    }
    let token = form_get(&body, "token").unwrap_or_default();
    let board = app.boards.get_mut(&id).unwrap();
    let Some(pos) = board.wires.iter().position(|w| w.token == token) else {
        return json(404, "Not Found", "{\"error\":\"gone\"}".into());
    };
    // Disarming forgets this wire's trips with it. The board's alarms_total
    // does not move: a count of what happened isn't the owner's to revise.
    board.wires.remove(pos);
    board.last_activity = now();
    app.token_index.remove(&token);
    json(200, "OK", "{\"ok\":true}".into())
}

fn api_board(app: &mut App, req: &Request) -> Response {
    let id = match live_board(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = verify_owner(app, &id, &form_get(&req.query, "owner").unwrap_or_default()) {
        return resp;
    }
    let t = now();
    let board = app.boards.get_mut(&id).unwrap();
    board.last_activity = t; // a watched board is a live board
    json(200, "OK", board_json(board))
}

/// SSE subscription for one board — owner-verified, like every other read.
/// No initial frame: the client fetches the board first, then attaches here
/// for the increments.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let id = match live_board(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return srv.respond(key, resp),
    };
    if let Err(resp) = verify_owner(app, &id, &form_get(&req.query, "owner").unwrap_or_default()) {
        return srv.respond(key, resp);
    }
    app.boards.get_mut(&id).unwrap().last_activity = now();
    srv.upgrade_sse(key, &id, "");
}
