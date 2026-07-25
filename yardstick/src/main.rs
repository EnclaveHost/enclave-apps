//! yardstick — measure a group without anyone showing their number.
//!
//! "What's your salary?" is the question everyone wants answered and nobody
//! wants to answer first. Same for consulting rates, rents, seed valuations,
//! sprint estimates. Every existing way to pool those numbers routes them
//! through someone — a spreadsheet owner, a survey vendor, that one trusted
//! colleague — who then *knows*. yardstick replaces the someone with attested
//! enclave RAM: each person submits one integer; at close the enclave reveals
//! only aggregate statistics, and only if at least k people submitted (k is
//! fixed at creation, floor 3); the individual numbers are scrubbed in the
//! same breath. Below quorum, *nothing* is revealed and everything is
//! scrubbed anyway — a threshold enforced by code someone can attest, not by
//! a promise.
//!
//! Honesty about what aggregates say: the median of three numbers IS the
//! middle person's number, and any quantile is *someone's* value — it just
//! doesn't say whose. That's inherent to aggregation, not a bug in it; pick
//! k to taste (the UI defaults to 5) and read the stats accordingly. And as
//! with any survey, the organizer can submit a skewing number — what they
//! can't do here is *see* yours.
//!
//! Submitters are anonymous to the server the way ballot's voters are: a
//! submission is keyed by the SHA-256 of a browser-held random token, and
//! re-submitting replaces your own number in place. At close each
//! participant can verify inclusion: the reveal publishes per-submission
//! fingerprints sha256(token_hash ":" value) — only the token holder can
//! recompute theirs. Logs carry counts, never ids or values.
//!
//! Aggregates revealed (all nearest-rank / exact integer math, replayable
//! against nothing — the inputs are gone; inclusion + attestation carry it):
//!   count, median, mean (one decimal), p25/p75 when count >= 5.
//!
//! API (bodies are `k=v&` forms, %-encoded; JSON is emit-only):
//!   POST /api/measures   headers x-measure-id, x-admin-hash, x-quorum,
//!                        x-unit?, x-ttl?, x-deadline-in?; body title=&desc=
//!   GET  /api/measure?id=  -> state; stats/fingerprints only when revealed
//!   POST /api/submit     id=&token=&value=   (re-submit replaces)
//!   POST /api/close      id=&admin=          reveal iff count >= quorum
//!   GET  /api/mine?id=&token=  -> your echo while open; counted after
//!   GET  /api/stream?id= -> SSE `subs` / `closed` on topic <id>
//!   GET  /api/stats      -> counts only       GET / , /m/<id>  UI

mod httpd;
mod sha256;

use httpd::{form_get, json, json_escape, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 16 * 1024;
const MAX_MEASURES: usize = 2_000;
const MAX_SUBS: usize = 10_000; // per measure
const MAX_TITLE: usize = 80; // chars
const MAX_DESC: usize = 500; // chars
const MAX_UNIT: usize = 12; // chars
const MAX_VALUE: u64 = 9_999_999_999_999; // 13 digits
const MIN_QUORUM: u32 = 3;
const MAX_QUORUM: u32 = 100;
const MIN_TTL: u64 = 3600;
const MAX_TTL: u64 = 30 * 24 * 3600;
const DEFAULT_TTL: u64 = 7 * 24 * 3600;
const MIN_DEADLINE: u64 = 60;
const CLOSED_LINGER: u64 = 7 * 24 * 3600; // revealed stats stay up this long

struct Measure {
    title: String,
    desc: String,
    unit: String,
    quorum: u32,
    subs: Vec<(String, u64)>, // (sha256 hex of token, value), insertion order
    by_token: HashMap<String, usize>, // token hash -> index into subs
    admin_hash: String,
    closed: bool,
    closed_at: Option<u64>,
    deadline: Option<u64>, // auto-close time, if the creator set one
    // Fixed at close, then the values above are scrubbed:
    revealed: bool, // count >= quorum at close
    count_at_close: u32,
    median_x10: u64,
    mean_x10: u64,
    p25: Option<u64>,
    p75: Option<u64>,
    fingerprints: Vec<String>, // sha256(token_hash ":" value) per slot
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    expires_at: u64,
}

struct App {
    measures: HashMap<String, Measure>,
    created: u64, // lifetime counters, for the stats line
    submitted: u64, // accepted submissions, replacements included
    closed: u64,
    revealed: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One submission's close-time fingerprint: sha256 over the UTF-8 of
/// "<token_hash_hex>:<value decimal>" — same shape as gavel's, for the same
/// reason: only the token holder can recompute theirs, so inclusion is
/// checkable while values stay unbruteforceable to everyone else.
fn fingerprint(token_hash: &str, value: u64) -> String {
    sha256::hex(format!("{token_hash}:{value}").as_bytes())
}

/// x10 fixed-point rendered with one decimal: 1235 -> "123.5".
fn fmt_x10(x: u64) -> String {
    format!("{}.{}", x / 10, x % 10)
}

fn main() {
    let mut srv = Server::bind("yardstick/0.1.0", 8080);
    let mut app = App {
        measures: HashMap::new(),
        created: 0,
        submitted: 0,
        closed: 0,
        revealed: 0,
        expired: 0,
    };
    let mut last_sweep = 0u64;
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            route(&mut app, &mut srv, key, &req);
        }
        let t = now();
        if t.saturating_sub(last_sweep) >= 15 {
            last_sweep = t;
            // Deadlines first: an open measure past its auto-close time goes
            // through the same close path a manual close takes.
            let due: Vec<String> = app
                .measures
                .iter()
                .filter(|(_, m)| !m.closed && m.deadline.is_some_and(|dl| dl <= t))
                .map(|(id, _)| id.clone())
                .collect();
            for id in due {
                close_measure(&mut app, &mut srv, &id);
            }
            let mut lapsed = 0u64;
            app.measures.retain(|_, m| match m.closed_at {
                Some(c) => c.saturating_add(CLOSED_LINGER) > t,
                None => {
                    let keep = m.expires_at > t;
                    if !keep {
                        lapsed += 1;
                    }
                    keep
                }
            });
            app.expired += lapsed;
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.created + app.submitted, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                let subs: usize = app.measures.values().map(|m| m.subs.len()).sum();
                println!(
                    "[yardstick] holding {} measures / {} sealed numbers; lifetime measures={} submissions={} closed={} revealed={} expired={}",
                    app.measures.len(),
                    subs,
                    app.created,
                    app.submitted,
                    app.closed,
                    app.revealed,
                    app.expired
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /m/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/m/") => {
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
            let subs: usize = app.measures.values().map(|m| m.subs.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"measures\":{},\"sealed_numbers\":{},\"lifetime_measures\":{},\"lifetime_submissions\":{},\"closed\":{},\"revealed\":{}}}",
                    app.measures.len(),
                    subs,
                    app.created,
                    app.submitted,
                    app.closed,
                    app.revealed
                ),
            )
        }
        ("POST", "/api/measures") => api_create(app, req),
        ("GET", "/api/measure") => api_measure(app, srv, req),
        ("POST", "/api/submit") => api_submit(app, srv, req),
        ("POST", "/api/close") => api_close(app, srv, req),
        ("GET", "/api/mine") => api_mine(app, srv, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Measure ids are client-generated: ^[a-z0-9]{8,16}$.
fn valid_id(id: &str) -> bool {
    (8..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Values arrive as ASCII decimal form fields; parse strictly. Zero is a
/// legal measurement (scores, counts), so the floor is 0.
fn parse_value(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 13 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u64 = s.parse().ok()?;
    (v <= MAX_VALUE).then_some(v)
}

fn clean_text(s: &str, max_chars: usize) -> Option<String> {
    let s = s.trim();
    (s.chars().count() <= max_chars && !s.chars().any(|c| (c as u32) < 0x20 && c != '\n'))
        .then(|| s.to_string())
}

fn api_create(app: &mut App, req: &Request) -> Response {
    let bad = |what: &str| json(400, "Bad Request", format!("{{\"error\":\"bad {what}\"}}"));
    let id = req.header("x-measure-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return bad("id");
    }
    let admin_hash = match req.header("x-admin-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return bad("admin hash"),
    };
    let quorum: u32 = match req.header("x-quorum").and_then(|v| v.parse().ok()) {
        Some(k) if (MIN_QUORUM..=MAX_QUORUM).contains(&k) => k,
        _ => return bad("quorum"),
    };
    let unit = match req.header("x-unit") {
        None => String::new(),
        Some(u) => match clean_text(u, MAX_UNIT) {
            Some(u) if !u.contains('\n') => u,
            _ => return bad("unit"),
        },
    };
    let ttl: u64 = match req.header("x-ttl") {
        None => DEFAULT_TTL,
        Some(v) => match v.parse() {
            Ok(t) if (MIN_TTL..=MAX_TTL).contains(&t) => t,
            _ => return bad("ttl"),
        },
    };
    let deadline_in: Option<u64> = match req.header("x-deadline-in") {
        None => None,
        Some(v) => match v.parse() {
            Ok(s) if (MIN_DEADLINE..=ttl).contains(&s) => Some(s),
            _ => return bad("deadline"),
        },
    };
    let body = String::from_utf8_lossy(&req.body).to_string();
    let title = match form_get(&body, "title").and_then(|t| clean_text(&t, MAX_TITLE)) {
        Some(t) if !t.is_empty() && !t.contains('\n') => t,
        _ => return bad("title"),
    };
    let desc = match form_get(&body, "desc") {
        None => String::new(),
        Some(d) => match clean_text(&d, MAX_DESC) {
            Some(d) => d,
            None => return bad("desc"),
        },
    };
    if app.measures.len() >= MAX_MEASURES {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.measures.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    let expires_at = t + ttl;
    app.created += 1;
    app.measures.insert(
        id,
        Measure {
            title,
            desc,
            unit,
            quorum,
            subs: Vec::new(),
            by_token: HashMap::new(),
            admin_hash,
            closed: false,
            closed_at: None,
            deadline: deadline_in.map(|s| t + s),
            revealed: false,
            count_at_close: 0,
            median_x10: 0,
            mean_x10: 0,
            p25: None,
            p75: None,
            fingerprints: Vec::new(),
            created: t,
            expires_at,
        },
    );
    json(
        200,
        "OK",
        format!("{{\"ok\":true,\"quorum\":{quorum},\"expires_at\":{expires_at}}}"),
    )
}

/// Resolve a measure a client may still address, closing it first if its
/// deadline passed between sweeps. Every other miss is the same 404.
fn live_measure(app: &mut App, srv: &mut Server, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) {
        return Err(gone());
    }
    match app.measures.get(id) {
        Some(m) if m.closed || m.expires_at > now() => {}
        Some(_) => {
            app.measures.remove(id);
            app.expired += 1;
            return Err(gone());
        }
        None => return Err(gone()),
    }
    let overdue = {
        let m = &app.measures[id];
        !m.closed && m.deadline.is_some_and(|dl| dl <= now())
    };
    if overdue {
        close_measure(app, srv, id);
    }
    Ok(id.to_string())
}

/// The one measure-state shape (/api/measure, the SSE initial event, the
/// closed broadcast). The keys that carry information — `median`, `mean`,
/// `p25`, `p75`, `fingerprints` — EXIST only once the measure closed AT
/// quorum: below it, close reveals nothing but the fact of failure.
fn measure_json(m: &Measure) -> String {
    let t = now();
    let closes_in = match (m.closed, m.deadline) {
        (false, Some(dl)) => dl.saturating_sub(t).to_string(),
        _ => "null".into(),
    };
    let expires_in = match m.closed_at {
        Some(c) => c.saturating_add(CLOSED_LINGER).saturating_sub(t),
        None => m.expires_at.saturating_sub(t),
    };
    let count = if m.closed {
        m.count_at_close as usize
    } else {
        m.subs.len()
    };
    let mut out = format!(
        "{{\"title\":\"{}\",\"desc\":\"{}\",\"unit\":\"{}\",\"quorum\":{},\"count\":{},\"closed\":{},\"closes_in\":{},\"expires_in\":{}",
        json_escape(&m.title),
        json_escape(&m.desc),
        json_escape(&m.unit),
        m.quorum,
        count,
        m.closed,
        closes_in,
        expires_in
    );
    if m.closed {
        out.push_str(&format!(",\"revealed\":{}", m.revealed));
        if m.revealed {
            let prints = m
                .fingerprints
                .iter()
                .map(|f| format!("\"{f}\""))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!(
                ",\"median\":{},\"mean\":{},\"p25\":{},\"p75\":{},\"fingerprints\":[{}]",
                fmt_x10(m.median_x10),
                fmt_x10(m.mean_x10),
                m.p25.map(|v| v.to_string()).unwrap_or("null".into()),
                m.p75.map(|v| v.to_string()).unwrap_or("null".into()),
                prints
            ));
        }
    }
    out.push('}');
    out
}

fn api_measure(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_measure(app, srv, &id) {
        Ok(id) => json(200, "OK", measure_json(&app.measures[&id])),
        Err(resp) => resp,
    }
}

fn api_submit(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_measure(app, srv, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if app.measures[&id].closed {
        return json(409, "Conflict", "{\"error\":\"closed\"}".into());
    }
    let token = form_get(&body, "token").unwrap_or_default();
    if !(16..=64).contains(&token.len()) {
        return json(400, "Bad Request", "{\"error\":\"bad token\"}".into());
    }
    let value = match parse_value(&form_get(&body, "value").unwrap_or_default()) {
        Some(v) => v,
        None => return json(400, "Bad Request", "{\"error\":\"bad value\"}".into()),
    };
    // The submission's key is the hash of a token only this browser knows;
    // the same token replaces its own number in place, a new one appends.
    let token_hash = sha256::hex(token.as_bytes());
    let m = app.measures.get_mut(&id).unwrap();
    match m.by_token.get(&token_hash) {
        Some(&i) => m.subs[i].1 = value,
        None => {
            if m.subs.len() >= MAX_SUBS {
                return json(507, "Insufficient Storage", "{\"error\":\"measure full\"}".into());
            }
            m.by_token.insert(token_hash.clone(), m.subs.len());
            m.subs.push((token_hash, value));
        }
    }
    let count = m.subs.len();
    app.submitted += 1;
    // Everyone watching learns participation, never values.
    srv.broadcast(&id, &format!("event: subs\ndata: {{\"count\":{count}}}"));
    json(200, "OK", format!("{{\"ok\":true,\"count\":{count}}}"))
}

/// The one close path — /api/close, the deadline sweep, and a too-late
/// submission all land here. Reveal iff quorum was met; either way, every
/// individual value is scrubbed from RAM in the same pass. Idempotent.
fn close_measure(app: &mut App, srv: &mut Server, id: &str) {
    let Some(m) = app.measures.get_mut(id) else { return };
    if m.closed {
        return;
    }
    let n = m.subs.len();
    m.count_at_close = n as u32;
    if n >= m.quorum as usize {
        let mut vals: Vec<u64> = m.subs.iter().map(|(_, v)| *v).collect();
        vals.sort_unstable();
        // Median: middle value, or the mean of the two middles (x.5 exact).
        m.median_x10 = if n % 2 == 1 {
            vals[n / 2] * 10
        } else {
            (vals[n / 2 - 1] + vals[n / 2]) * 5
        };
        // Mean to one decimal, round-half-up in exact integer math.
        let sum: u64 = vals.iter().sum();
        m.mean_x10 = (sum * 10 + n as u64 / 2) / n as u64;
        // Quartiles by nearest rank (1-based ceil(q*n)), only at n >= 5 —
        // below that they'd bracket nearly every individual number.
        if n >= 5 {
            m.p25 = Some(vals[(n + 3) / 4 - 1]);
            m.p75 = Some(vals[(3 * n + 3) / 4 - 1]);
        }
        m.fingerprints = m
            .subs
            .iter()
            .map(|(th, v)| fingerprint(th, *v))
            .collect();
        m.revealed = true;
        app.revealed += 1;
    }
    // Scrub the numbers either way: token hashes stay (so /api/mine can say
    // "you were counted"), values cease to exist.
    for s in m.subs.iter_mut() {
        s.1 = 0;
    }
    m.closed = true;
    m.closed_at = Some(now());
    app.closed += 1;
    let payload = measure_json(&app.measures[id]);
    srv.broadcast(id, &format!("event: closed\ndata: {payload}"));
}

fn api_close(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_measure(app, srv, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let admin = form_get(&body, "admin").unwrap_or_default();
    if admin.is_empty() || sha256::hex(admin.as_bytes()) != app.measures[&id].admin_hash {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    close_measure(app, srv, &id);
    json(200, "OK", measure_json(&app.measures[&id]))
}

/// A submitter's private echo, keyed by their token. Open: your recorded
/// value, proof the seal took. Closed: whether you were counted — the value
/// itself no longer exists anywhere to be echoed.
fn api_mine(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let id = match live_measure(app, srv, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let token = form_get(&req.query, "token").unwrap_or_default();
    let token_hash = sha256::hex(token.as_bytes());
    let m = &app.measures[&id];
    if !m.closed {
        let value = m
            .by_token
            .get(&token_hash)
            .map(|&i| m.subs[i].1.to_string())
            .unwrap_or("null".into());
        return json(200, "OK", format!("{{\"closed\":false,\"value\":{value}}}"));
    }
    let counted = m.by_token.contains_key(&token_hash);
    json(
        200,
        "OK",
        format!("{{\"closed\":true,\"counted\":{counted}}}"),
    )
}

/// SSE subscription for one measure. Late joiners get the current state as
/// the first (unnamed) event, framed exactly like /api/measure.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_measure(app, srv, &id) {
        Ok(id) => {
            let initial = format!("data: {}\n\n", measure_json(&app.measures[&id]));
            srv.upgrade_sse(key, &id, &initial);
        }
        Err(resp) => srv.respond(key, resp),
    }
}
