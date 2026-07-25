//! pulse — push-based uptime / heartbeat monitoring as an Enclave service app.
//!
//! Monitoring, inverted — which is exactly the shape a no-egress enclave
//! wants: this server never probes anything. Your cron jobs, backup scripts
//! and services curl a per-monitor heartbeat URL INTO the enclave on their
//! own schedule, and the status page turns red when the beats stop. The
//! server stores only `{page -> monitors, beat arrival times}` in memory
//! and derives every status from arrivals alone: never beat -> waiting,
//! within the period -> up, inside the grace -> late, past it -> down.
//!
//! The TEE angle, stated honestly: the beat log lives in attested code and
//! enclave RAM. Nobody — including whoever hosts the page — can backfill a
//! missed beat, edit history, or quietly drop an embarrassing gap. What's
//! green was actually beating. (What no TEE can prove: that your job was
//! healthy — only that it called in. And timing trusts the platform clock,
//! the same trust billing already requires.)
//!
//! Beat URLs are capability URLs: whoever holds one can feed that monitor,
//! nobody else can. They never appear on the public page, in the stream,
//! in logs or in stats; the owner token (checked against its SHA-256) is
//! the only way to read one back.
//!
//! API (bodies are `k=v&` forms; JSON is emit-only):
//!   POST /api/pages            headers x-page-id, x-owner-hash        -> {ok}
//!   POST /api/monitors         id=&owner=&name=&period=&grace=&beat=  -> {ok}
//!   POST /api/monitors/remove  id=&owner=&beat=                       -> {ok}
//!   GET|POST /hb/<beat-token>  the heartbeat -> {ok, next_due} + SSE fan-out
//!   GET  /api/page?id=<id>[&owner=<token>]  -> statuses (owner: + beat tokens)
//!   GET  /api/stream?id=<id>   -> SSE `beat` / `page` on topic <page id>
//!   GET  /api/stats            -> counts only, never ids or tokens
//!   GET  / , /s/<id>   UI      GET /ping   liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, json_escape, Request, Response, Server};
use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 8 * 1024;
const MAX_PAGES: usize = 500;
const MAX_MONITORS: usize = 20; // per page
const RECENT_CAP: usize = 50; // beat timestamps kept per monitor
const GAPS_SHOWN: usize = 12; // inter-beat gaps emitted for the regularity strip
const MIN_PERIOD: u64 = 60;
const MAX_PERIOD: u64 = 86_400;
const MAX_GRACE: u64 = 86_400;
const PAGE_TTL: u64 = 30 * 24 * 3600; // idle time before a page is swept

struct Monitor {
    name: String, // display text, not an identifier; json_escape on emit
    period: u64,
    grace: u64,
    beat_token: String, // client-chosen capability, dead-drop's id charset
    last_beat: Option<u64>,
    total_beats: u64,
    recent: VecDeque<u64>, // last RECENT_CAP beat timestamps, oldest first
}

struct Page {
    owner_hash: String, // sha256 hex — the add/remove/read-back credential
    monitors: Vec<Monitor>,
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    last_activity: u64,  // any beat, edit, or stream attach
    last_state_hash: u64, // ordered statuses at the last change tick
}

struct App {
    pages: HashMap<String, Page>,
    // The hot path is a cron job's curl: beat token -> page id must be one
    // hash lookup, so this index is kept in sync on add/remove/sweep.
    beat_index: HashMap<String, String>,
    beats_total: u64, // lifetime counters, for the stats line
    lifetime_pages: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("pulse/0.1.0", 8080);
    let mut app = App {
        pages: HashMap::new(),
        beat_index: HashMap::new(),
        beats_total: 0,
        lifetime_pages: 0,
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
        if t.saturating_sub(last_tick) >= 15 {
            last_tick = t;
            // The change tick: beats announce themselves, but sliding to
            // late or down is the passage of time — nobody calls in to say
            // so. Any page whose ordered statuses moved since the last
            // tick gets a full refresh event on its topic.
            let mut changed: Vec<(String, String)> = Vec::new();
            for (id, page) in app.pages.iter_mut() {
                let h = state_hash(page, t);
                if h != page.last_state_hash {
                    page.last_state_hash = h;
                    changed.push((
                        id.clone(),
                        format!("event: page\ndata: {}", page_json(page, t, false)),
                    ));
                }
            }
            for (id, event) in changed {
                srv.broadcast(&id, &event);
            }
        }
        if t.saturating_sub(last_sweep) >= 60 {
            last_sweep = t;
            let dead: Vec<String> = app
                .pages
                .iter()
                .filter(|(_, p)| p.last_activity + PAGE_TTL < t)
                .map(|(id, _)| id.clone())
                .collect();
            for id in dead {
                let p = app.pages.remove(&id).unwrap();
                for m in &p.monitors {
                    app.beat_index.remove(&m.beat_token);
                }
                app.expired += 1;
            }
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.lifetime_pages + app.beats_total, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[pulse] watching {} pages / {} monitors; lifetime pages={} beats={} expired={}",
                    app.pages.len(),
                    app.beat_index.len(),
                    app.lifetime_pages,
                    app.beats_total,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    // The heartbeat takes GET and POST alike: whatever a cron line's curl
    // emits, the beat counts. Query strings and bodies are ignored.
    if let Some(token) = req.path.strip_prefix("/hb/") {
        if req.method == "GET" || req.method == "POST" {
            let resp = api_hb(app, srv, token);
            srv.respond(key, resp);
            return;
        }
    }
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /s/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/s/") => {
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
            let monitors: usize = app.pages.values().map(|p| p.monitors.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"pages\":{},\"monitors\":{},\"beats_total\":{},\"lifetime_pages\":{}}}",
                    app.pages.len(),
                    monitors,
                    app.beats_total,
                    app.lifetime_pages
                ),
            )
        }
        ("POST", "/api/pages") => api_pages(app, req),
        ("POST", "/api/monitors") => api_monitors(app, req),
        ("POST", "/api/monitors/remove") => api_monitors_remove(app, req),
        ("GET", "/api/page") => api_page(app, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Page ids are client-generated: ^[a-z0-9]{10,16}$.
fn valid_page_id(id: &str) -> bool {
    (10..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Beat tokens are client-generated too, in dead-drop's id charset.
fn valid_token(token: &str) -> bool {
    (16..=43).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Monitor names are display text: 1..=60 chars, anything printable —
/// json_escape carries the emit side.
fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=60).contains(&n) && !name.chars().any(|c| c.is_control())
}

/// The one derivation the whole app turns on, shared by every emit path and
/// the change tick: a status comes from beat arrival times and nothing else.
fn status(m: &Monitor, t: u64) -> &'static str {
    match m.last_beat {
        None => "waiting",
        Some(b) if t <= b + m.period => "up",
        Some(b) if t <= b + m.period + m.grace => "late",
        Some(_) => "down",
    }
}

/// One monitor as the public page sees it. `with_token` is the owner's view:
/// a beat URL is a capability, so it rides only owner-verified responses —
/// never the public JSON, never the stream.
fn monitor_json(m: &Monitor, t: u64, with_token: bool) -> String {
    let last_beat_ago = match m.last_beat {
        Some(b) => t.saturating_sub(b).to_string(),
        None => "null".into(),
    };
    let gaps: Vec<u64> = m
        .recent
        .iter()
        .zip(m.recent.iter().skip(1))
        .map(|(a, b)| b.saturating_sub(*a))
        .collect();
    let gaps = gaps[gaps.len().saturating_sub(GAPS_SHOWN)..]
        .iter()
        .map(|g| g.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let token = if with_token {
        format!(",\"beat_token\":\"{}\"", m.beat_token)
    } else {
        String::new()
    };
    format!(
        "{{\"name\":\"{}\",\"period\":{},\"grace\":{},\"status\":\"{}\",\"last_beat_ago\":{},\"total_beats\":{},\"recent_gaps\":[{}]{}}}",
        json_escape(&m.name),
        m.period,
        m.grace,
        status(m, t),
        last_beat_ago,
        m.total_beats,
        gaps,
        token
    )
}

fn page_json(p: &Page, t: u64, with_tokens: bool) -> String {
    let monitors = p
        .monitors
        .iter()
        .map(|m| monitor_json(m, t, with_tokens))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"monitors\":[{}],\"count\":{}}}", monitors, p.monitors.len())
}

/// FNV-1a over the page's ordered status strings — cheap enough to run for
/// every page every tick, and any transition (or an add/remove) moves it.
fn state_hash(p: &Page, t: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for m in &p.monitors {
        for b in status(m, t).bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        h = (h ^ 0x1f).wrapping_mul(0x100_0000_01b3); // monitor separator
    }
    h
}

fn api_pages(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-page-id").unwrap_or("").to_string();
    if !valid_page_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let owner_hash = match req.header("x-owner-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return json(400, "Bad Request", "{\"error\":\"bad owner hash\"}".into()),
    };
    if app.pages.len() >= MAX_PAGES {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.pages.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    let mut page = Page {
        owner_hash,
        monitors: Vec::new(),
        created: t,
        last_activity: t,
        last_state_hash: 0,
    };
    page.last_state_hash = state_hash(&page, t);
    app.lifetime_pages += 1;
    app.pages.insert(id, page);
    json(200, "OK", "{\"ok\":true}".into())
}

/// Look up a live page by id; misses of every kind (bad id, unknown,
/// expired-but-unswept) are ONE indistinguishable 404.
fn live_page(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_page_id(id) {
        return Err(gone());
    }
    match app.pages.get(id) {
        Some(p) if p.last_activity + PAGE_TTL >= now() => Ok(id.to_string()),
        Some(_) => {
            let p = app.pages.remove(id).unwrap();
            for m in &p.monitors {
                app.beat_index.remove(&m.beat_token);
            }
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

/// Check the form's `owner=` token against the page's stored hash.
fn verify_owner(app: &App, id: &str, body: &str) -> Result<(), Response> {
    let token = form_get(body, "owner").unwrap_or_default();
    if token.is_empty() || sha256::hex(token.as_bytes()) != app.pages[id].owner_hash {
        return Err(json(403, "Forbidden", "{\"error\":\"bad token\"}".into()));
    }
    Ok(())
}

fn api_monitors(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_page(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = verify_owner(app, &id, &body) {
        return resp;
    }
    let name = form_get(&body, "name").unwrap_or_default();
    if !valid_name(&name) {
        return json(400, "Bad Request", "{\"error\":\"bad name\"}".into());
    }
    let period: u64 = match form_get(&body, "period").and_then(|v| v.parse().ok()) {
        Some(p) if (MIN_PERIOD..=MAX_PERIOD).contains(&p) => p,
        _ => return json(400, "Bad Request", "{\"error\":\"bad period\"}".into()),
    };
    let grace: u64 = match form_get(&body, "grace").and_then(|v| v.parse().ok()) {
        Some(g) if g <= MAX_GRACE => g,
        _ => return json(400, "Bad Request", "{\"error\":\"bad grace\"}".into()),
    };
    let beat = form_get(&body, "beat").unwrap_or_default();
    if !valid_token(&beat) {
        return json(400, "Bad Request", "{\"error\":\"bad beat token\"}".into());
    }
    if app.pages[&id].monitors.len() >= MAX_MONITORS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"monitor cap reached\"}".into(),
        );
    }
    // Beat tokens are global capabilities: one token, one monitor, ever —
    // the O(1) hot path depends on the index staying a bijection.
    if app.beat_index.contains_key(&beat) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let page = app.pages.get_mut(&id).unwrap();
    page.last_activity = now();
    page.monitors.push(Monitor {
        name,
        period,
        grace,
        beat_token: beat.clone(),
        last_beat: None,
        total_beats: 0,
        recent: VecDeque::new(),
    });
    app.beat_index.insert(beat, id);
    json(200, "OK", "{\"ok\":true}".into())
}

fn api_monitors_remove(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_page(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if let Err(resp) = verify_owner(app, &id, &body) {
        return resp;
    }
    let beat = form_get(&body, "beat").unwrap_or_default();
    let page = app.pages.get_mut(&id).unwrap();
    let Some(pos) = page.monitors.iter().position(|m| m.beat_token == beat) else {
        return json(404, "Not Found", "{\"error\":\"gone\"}".into());
    };
    page.monitors.remove(pos);
    page.last_activity = now();
    app.beat_index.remove(&beat);
    json(200, "OK", "{\"ok\":true}".into())
}

/// THE hot path: a cron job's curl. One hash lookup, one scan of a ≤20-
/// monitor page, one broadcast. Misses of every kind are ONE uniform 404 —
/// a beat URL proves nothing unless it's live.
fn api_hb(app: &mut App, srv: &mut Server, token: &str) -> Response {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    let Some(id) = app.beat_index.get(token).cloned() else {
        return gone();
    };
    let t = now();
    let Some(page) = app.pages.get_mut(&id) else {
        return gone();
    };
    if page.last_activity + PAGE_TTL < t {
        return gone(); // expired-but-unswept; the sweeper will erase it
    }
    let Some(m) = page.monitors.iter_mut().find(|m| m.beat_token == token) else {
        return gone();
    };
    m.last_beat = Some(t);
    m.total_beats += 1;
    m.recent.push_back(t);
    if m.recent.len() > RECENT_CAP {
        m.recent.pop_front();
    }
    let next_due = t + m.period;
    let event = format!("event: beat\ndata: {}", monitor_json(m, t, false));
    page.last_activity = t;
    app.beats_total += 1;
    srv.broadcast(&id, &event);
    json(200, "OK", format!("{{\"ok\":true,\"next_due\":{next_due}}}"))
}

fn api_page(app: &mut App, req: &Request) -> Response {
    let id = match live_page(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let with_tokens = match form_get(&req.query, "owner") {
        None => false,
        Some(token) => {
            if token.is_empty() || sha256::hex(token.as_bytes()) != app.pages[&id].owner_hash {
                return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
            }
            true
        }
    };
    json(200, "OK", page_json(&app.pages[&id], now(), with_tokens))
}

/// SSE subscription for one page. The initial frame is the public page
/// JSON — the same bytes /api/page serves — so a subscriber renders once
/// and then applies increments.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    match live_page(app, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => {
            let t = now();
            let page = app.pages.get_mut(&id).unwrap();
            page.last_activity = t; // a watched page is a live page
            let initial = format!("data: {}\n\n", page_json(page, t, false));
            srv.upgrade_sse(key, &id, &initial);
        }
        Err(resp) => srv.respond(key, resp),
    }
}
