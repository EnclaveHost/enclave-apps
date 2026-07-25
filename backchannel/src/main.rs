//! backchannel — e2e-encrypted ephemeral chat rooms relayed blind through a TEE.
//!
//! The server is a relay that CANNOT read the room: the room key rides the
//! invite link's URL FRAGMENT (never sent to any server), every message
//! arrives as AES-256-GCM ciphertext sealed in the browser, and the nickname
//! travels INSIDE the plaintext — so this process never learns what was said
//! or even who said it. It keeps a 200-message ring per room in enclave RAM,
//! fans ciphertext out to whoever is subscribed, and forgets: a room dies
//! two hours after its last message, and nothing ever touches a disk.
//! Running inside an attested TEE is what makes "blind relay" checkable —
//! no operator shell, no logs of content, a build reproducible from this
//! source via the on-chain catalog.
//!
//! What the server never learns: the key, the plaintext, the nicknames, who
//! is in a room. What it does see: opaque blobs, their sizes, their timing,
//! and how many streams are open — the irreducible metadata of any relay.
//! What it refuses to say: whether a room id never existed or already died —
//! every miss is the same 404.
//!
//! API (bodies are opaque blobs or `k=v&` forms; JSON is emit-only):
//!   POST /api/rooms      header x-room-id           -> {ok}
//!   POST /api/msg        id=<room>&blob=<b64u>      -> {ok, n} + SSE fan-out
//!   GET  /api/history?id=<room>                     -> {seq, msgs[]} oldest first
//!   GET  /api/stream?id=<room>                      -> SSE `msg` / `present` on topic <id>
//!   GET  /api/stats                                 -> counts only, never ids
//!   GET  / , /r/<id>     UI         GET /ping       liveness

mod httpd;

use httpd::{form_get, json, Request, Response, Server};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BLOB: usize = 8 * 1024; // base64url chars per message (~6 KiB ciphertext)
const MAX_BODY: usize = 12 * 1024;
const MAX_ROOMS: usize = 2_000;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const RING_CAP: usize = 200; // messages held per room; older ones fall off
const ROOM_TTL: u64 = 2 * 3600; // a room dies this long after its last message

struct Msg {
    n: u64,
    ts: u64,
    blob: String, // opaque base64url ciphertext; never parsed, never logged
}

struct Room {
    ring: VecDeque<Msg>,
    seq: u64, // total messages ever relayed here; ring holds the tail
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    last_activity: u64,
    last_present: usize, // last presence count broadcast to this room
}

struct App {
    rooms: HashMap<String, Room>,
    total_bytes: usize,
    opened: u64, // lifetime counters, for the stats line
    relayed: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("backchannel/0.1.2", 8080);
    let mut app = App {
        rooms: HashMap::new(),
        total_bytes: 0,
        opened: 0,
        relayed: 0,
        expired: 0,
    };
    let mut last_tick = 0u64;
    let mut last_sweep = 0u64;
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            route(&mut app, &mut srv, key, &req);
        }
        let t = now();
        if t.saturating_sub(last_tick) >= 5 {
            last_tick = t;
            // Presence: every room with an audience hears the count when it
            // changes — never who is here, only how many streams are open.
            for (id, room) in app.rooms.iter_mut() {
                let n = srv.sse_count(id);
                if n > 0 && room.last_present != n {
                    room.last_present = n;
                    srv.broadcast(id, &format!("event: present\ndata: {{\"present\":{n}}}"));
                }
            }
        }
        if t.saturating_sub(last_sweep) >= 30 {
            last_sweep = t;
            let before = app.rooms.len();
            app.rooms.retain(|_, r| r.last_activity + ROOM_TTL >= t);
            let swept = before - app.rooms.len();
            if swept > 0 {
                app.expired += swept as u64;
                app.total_bytes = app
                    .rooms
                    .values()
                    .flat_map(|r| r.ring.iter())
                    .map(|m| m.blob.len())
                    .sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.opened + app.relayed, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                let held: usize = app.rooms.values().map(|r| r.ring.len()).sum();
                println!(
                    "[backchannel] holding {} rooms / {} messages ({} KiB); lifetime rooms={} messages={} expired={}",
                    app.rooms.len(),
                    held,
                    app.total_bytes / 1024,
                    app.opened,
                    app.relayed,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /r/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/r/") => {
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
            let held: usize = app.rooms.values().map(|r| r.ring.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"rooms\":{},\"messages\":{},\"lifetime_rooms\":{},\"lifetime_messages\":{}}}",
                    app.rooms.len(),
                    held,
                    app.opened,
                    app.relayed
                ),
            )
        }
        ("POST", "/api/rooms") => api_rooms(app, req),
        ("POST", "/api/leave") => api_leave(app, srv, req),
        ("POST", "/api/msg") => api_msg(app, srv, req),
        ("GET", "/api/history") => api_history(app, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Room ids are client-generated: ^[a-z0-9]{10,16}$.
fn valid_id(id: &str) -> bool {
    (10..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn api_rooms(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-room-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    if app.rooms.len() >= MAX_ROOMS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.rooms.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    app.opened += 1;
    app.rooms.insert(
        id,
        Room {
            ring: VecDeque::new(),
            seq: 0,
            created: t,
            last_activity: t,
            last_present: 0,
        },
    );
    json(200, "OK", "{\"ok\":true}".into())
}

/// Look up a live room by id; misses of every kind (bad id, unknown,
/// expired-but-unswept) are ONE indistinguishable 404.
fn live_room(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) {
        return Err(gone());
    }
    match app.rooms.get(id) {
        Some(r) if r.last_activity + ROOM_TTL >= now() => Ok(id.to_string()),
        Some(_) => {
            let r = app.rooms.remove(id).unwrap();
            app.total_bytes -= r.ring.iter().map(|m| m.blob.len()).sum::<usize>();
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

fn api_msg(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_room(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let blob = form_get(&body, "blob").unwrap_or_default();
    if blob.is_empty()
        || blob.len() > MAX_BLOB
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return json(400, "Bad Request", "{\"error\":\"bad blob\"}".into());
    }
    let room = app.rooms.get_mut(&id).unwrap();
    // A full ring frees its oldest slot, so only NET growth counts toward
    // the byte cap; at capacity we say so rather than evicting other rooms.
    let freed = if room.ring.len() >= RING_CAP {
        room.ring.front().map(|m| m.blob.len()).unwrap_or(0)
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
    if room.ring.len() >= RING_CAP {
        let old = room.ring.pop_front().unwrap();
        app.total_bytes -= old.blob.len();
    }
    room.seq += 1;
    let n = room.seq;
    let ts = now();
    room.last_activity = ts;
    app.total_bytes += blob.len();
    app.relayed += 1;
    let event = format!("event: msg\ndata: {{\"n\":{n},\"ts\":{ts},\"blob\":\"{blob}\"}}");
    room.ring.push_back(Msg { n, ts, blob });
    srv.broadcast(&id, &event);
    json(200, "OK", format!("{{\"ok\":true,\"n\":{n}}}"))
}

fn api_history(app: &mut App, req: &Request) -> Response {
    let id = match live_room(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let room = &app.rooms[&id];
    let msgs = room
        .ring
        .iter()
        .map(|m| format!("{{\"n\":{},\"ts\":{},\"blob\":\"{}\"}}", m.n, m.ts, m.blob))
        .collect::<Vec<_>>()
        .join(",");
    json(200, "OK", format!("{{\"seq\":{},\"msgs\":[{}]}}", room.seq, msgs))
}

/// SSE subscription for one room. The first (unnamed) event tells the
/// subscriber how many are here — itself included, since the count is taken
/// a beat before this connection becomes a subscriber.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    match live_room(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => {
            let present = srv.sse_count(&id) + 1; // +1: the subscriber itself
            let initial = format!("data: {{\"present\":{present}}}\n\n");
            srv.upgrade_sse(key, &id, &initial);
            // An opaque per-tab stream token lets a leave beacon name this
            // exact stream later — a proxy hop may hold the socket open
            // long after the tab is gone, and presence shouldn't lie.
            if let Some(tag) = form_get(&req.query, "s").filter(|t| valid_tag(t)) {
                srv.tag_sse(key, &tag);
            }
        }
        Err(resp) => srv.respond(key, resp),
    }
}

/// Stream tokens are whatever opaque random string the browser minted:
/// never stored, never logged, compared only to close the right stream.
fn valid_tag(t: &str) -> bool {
    (8..=64).contains(&t.len())
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `sendBeacon` on pagehide: close the named stream now instead of waiting
/// for a proxy to notice the tab died. Fire-and-forget by design — the
/// answer is 200 whatever happened, so a beacon can't probe room ids.
fn api_leave(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = form_get(&body, "id").unwrap_or_default();
    let tag = form_get(&body, "s").unwrap_or_default();
    if valid_id(&id) && valid_tag(&tag) && srv.drop_sse(&id, &tag) > 0 {
        // The count changed this instant; tell the room now, not next tick.
        let n = srv.sse_count(&id);
        if let Some(room) = app.rooms.get_mut(&id) {
            room.last_present = n;
            if n > 0 {
                srv.broadcast(&id, &format!("event: present\ndata: {{\"present\":{n}}}"));
            }
        }
    }
    json(200, "OK", "{\"ok\":true}".into())
}
