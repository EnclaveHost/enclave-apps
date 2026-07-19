//! warpad — an e2e-encrypted shared scratchpad relayed blind through a TEE.
//!
//! The server is a version counter for ONE opaque ciphertext per pad: the
//! pad key rides the invite link's URL FRAGMENT (never sent to any server),
//! the document only ever exists here as a single AES-256-GCM blob sealed
//! in a browser, and every save REPLACES that blob whole under an
//! incrementing version. That makes "no history" structural, not policy:
//! there is no ring, no journal, no undo buffer — the previous draft stops
//! existing the moment a save lands. Saves are optimistic: a stale base
//! version gets a 409 carrying the current state, and the CLIENT rebases —
//! the server can't merge what it can't read. Everyone on the pad hears
//! each save within a beat over SSE; a pad dies 24 hours after its last
//! write, and nothing ever touches a disk. Running inside an attested TEE
//! is what makes "blind" checkable — no operator shell, no logs of content,
//! a build reproducible from this source via the on-chain catalog.
//!
//! What the server never learns: the key, the text, who is writing. What
//! it does see: one blob per pad, its size, its version, and how many
//! streams are open — the irreducible metadata of any relay. What it
//! refuses to say: whether a pad id never existed or already died — every
//! miss is the same 404.
//!
//! API (bodies are opaque blobs or `k=v&` forms; JSON is emit-only):
//!   POST /api/pads       header x-pad-id                -> {ok}
//!   POST /api/leave      id=<pad>&s=<tag>               -> {ok} always (beacon)
//!   POST /api/save       id=<pad>&v=<base>&blob=<b64u>  -> {ok, version} | 409 {version, blob}
//!   GET  /api/pad?id=<pad>                              -> {version, blob, expires_in}
//!   GET  /api/stream?id=<pad>                           -> SSE `doc` / `present` on topic <id>
//!   GET  /api/stats                                     -> counts only, never ids
//!   GET  / , /p/<id>     UI         GET /ping           liveness

mod httpd;

use httpd::{form_get, json, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BLOB: usize = 256 * 1024; // base64url chars per pad (~192 KiB ciphertext)
const MAX_BODY: usize = 300 * 1024;
const MAX_PADS: usize = 1_000;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const PAD_TTL: u64 = 24 * 3600; // a pad dies this long after its last write

struct Pad {
    blob: String, // opaque base64url ciphertext; never parsed, never logged
    version: u64, // bumped on every save; the client's optimistic-lock token
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    last_write: u64,
    last_present: usize, // last presence count broadcast to this pad
}

struct App {
    pads: HashMap<String, Pad>,
    total_bytes: usize,
    opened: u64, // lifetime counters, for the stats line
    saved: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("warpad/0.1.1", 8080);
    let mut app = App {
        pads: HashMap::new(),
        total_bytes: 0,
        opened: 0,
        saved: 0,
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
            // Presence: every pad with an audience hears the count when it
            // changes — never who is here, only how many streams are open.
            // The count is tracked even down to zero (silently: nobody is
            // left to tell), so the next arrival always hears a fresh "1".
            for (id, pad) in app.pads.iter_mut() {
                let n = srv.sse_count(id);
                if n != pad.last_present {
                    pad.last_present = n;
                    if n > 0 {
                        srv.broadcast(id, &format!("event: present\ndata: {{\"present\":{n}}}"));
                    }
                }
            }
        }
        if t.saturating_sub(last_sweep) >= 60 {
            last_sweep = t;
            let before = app.pads.len();
            app.pads.retain(|_, p| p.last_write + PAD_TTL >= t);
            let swept = before - app.pads.len();
            if swept > 0 {
                app.expired += swept as u64;
                app.total_bytes = app.pads.values().map(|p| p.blob.len()).sum();
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.opened + app.saved, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[warpad] holding {} pads ({} KiB); lifetime pads={} saves={} expired={}",
                    app.pads.len(),
                    app.total_bytes / 1024,
                    app.opened,
                    app.saved,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /p/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/p/") => {
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
                "{{\"pads\":{},\"bytes\":{},\"lifetime_pads\":{},\"lifetime_saves\":{}}}",
                app.pads.len(),
                app.total_bytes,
                app.opened,
                app.saved
            ),
        ),
        ("POST", "/api/pads") => api_pads(app, req),
        ("POST", "/api/leave") => api_leave(app, srv, req),
        ("POST", "/api/save") => api_save(app, srv, req),
        ("GET", "/api/pad") => api_pad(app, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Pad ids are client-generated: ^[a-z0-9]{10,16}$.
fn valid_id(id: &str) -> bool {
    (10..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn api_pads(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-pad-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    if app.pads.len() >= MAX_PADS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.pads.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    app.opened += 1;
    // A new pad is version 0 with an empty blob: "nothing written yet" and
    // "cleared" are the same honest state.
    app.pads.insert(
        id,
        Pad {
            blob: String::new(),
            version: 0,
            created: t,
            last_write: t,
            last_present: 0,
        },
    );
    json(200, "OK", "{\"ok\":true}".into())
}

/// Look up a live pad by id; misses of every kind (bad id, unknown,
/// expired-but-unswept) are ONE indistinguishable 404.
fn live_pad(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) {
        return Err(gone());
    }
    match app.pads.get(id) {
        Some(p) if p.last_write + PAD_TTL >= now() => Ok(id.to_string()),
        Some(_) => {
            let p = app.pads.remove(id).unwrap();
            app.total_bytes -= p.blob.len();
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

fn api_save(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_pad(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let Some(v) = form_get(&body, "v").and_then(|s| s.parse::<u64>().ok()) else {
        return json(400, "Bad Request", "{\"error\":\"bad version\"}".into());
    };
    // Empty is legal — clearing the pad is a save like any other.
    let blob = form_get(&body, "blob").unwrap_or_default();
    if blob.len() > MAX_BLOB
        || !blob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return json(400, "Bad Request", "{\"error\":\"bad blob\"}".into());
    }
    let pad = app.pads.get_mut(&id).unwrap();
    // Optimistic concurrency: a stale base version gets the current state
    // back and the CLIENT rebases — the server can't merge what it can't
    // read, and it will not silently drop someone's words on the floor.
    if v != pad.version {
        return json(
            409,
            "Conflict",
            format!(
                "{{\"error\":\"conflict\",\"version\":{},\"blob\":\"{}\"}}",
                pad.version, pad.blob
            ),
        );
    }
    // A save replaces the old blob, so only NET growth counts toward the
    // byte cap; at capacity we say so rather than evicting someone's pad.
    if app.total_bytes + blob.len() > MAX_TOTAL_BYTES + pad.blob.len() {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    app.total_bytes = app.total_bytes - pad.blob.len() + blob.len();
    pad.version += 1;
    pad.last_write = now();
    // The old ciphertext ends here, mid-function: this assignment IS the
    // no-history guarantee.
    pad.blob = blob;
    let version = pad.version;
    let event = format!(
        "event: doc\ndata: {{\"version\":{version},\"blob\":\"{}\"}}",
        pad.blob
    );
    app.saved += 1;
    srv.broadcast(&id, &event);
    json(200, "OK", format!("{{\"ok\":true,\"version\":{version}}}"))
}

fn api_pad(app: &mut App, req: &Request) -> Response {
    let id = match live_pad(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let p = &app.pads[&id];
    json(
        200,
        "OK",
        format!(
            "{{\"version\":{},\"blob\":\"{}\",\"expires_in\":{}}}",
            p.version,
            p.blob,
            (p.last_write + PAD_TTL).saturating_sub(now())
        ),
    )
}

/// SSE subscription for one pad. The first (unnamed) event is the current
/// document, so a subscriber renders without racing a separate fetch —
/// and a reconnect after any gap replays the latest state the same way.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    match live_pad(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => {
            let p = &app.pads[&id];
            let initial = format!(
                "data: {{\"version\":{},\"blob\":\"{}\"}}\n\n",
                p.version, p.blob
            );
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
/// answer is 200 whatever happened, so a beacon can't probe pad ids.
fn api_leave(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = form_get(&body, "id").unwrap_or_default();
    let tag = form_get(&body, "s").unwrap_or_default();
    if valid_id(&id) && valid_tag(&tag) && srv.drop_sse(&id, &tag) > 0 {
        // The count changed this instant; tell the pad now, not next tick —
        // and record it, so the tick doesn't re-broadcast a stale value.
        let n = srv.sse_count(&id);
        if let Some(pad) = app.pads.get_mut(&id) {
            pad.last_present = n;
            if n > 0 {
                srv.broadcast(&id, &format!("event: present\ndata: {{\"present\":{n}}}"));
            }
        }
    }
    json(200, "OK", "{\"ok\":true}".into())
}
