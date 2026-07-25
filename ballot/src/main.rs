//! ballot — anonymous polls whose tally stays sealed in a TEE until close.
//!
//! Honest polling has two failure modes software alone can't rule out: the
//! creator peeks mid-vote and times the close, and early numbers herd the
//! late voters. **sealed** mode (the default) removes both by construction:
//! between the first ballot and the close, the counts exist ONLY in enclave
//! RAM, and the attested code holding them refuses to show a tally to
//! anyone — the creator included. **live** mode is the ordinary kind,
//! tallies moving on screen, for when the bandwagon is the fun part.
//!
//! Ballots are anonymous in both modes: a voter IS the SHA-256 of a random
//! token their browser generated. No name, no cookie, no address — the
//! platform's in-TEE proxy is the only thing that ever saw the TCP peer.
//! Presenting the same token again moves the ballot (one token, one vote);
//! the hash of the creator's admin token, fixed at creation, is the only
//! key that closes a poll and breaks the seal.
//!
//! What the server never learns: who voted, which ballot is whose. What it
//! never says: a sealed tally before close. Logs carry counts — never ids,
//! questions or choices.
//!
//! API (bodies are percent-encoded lines or `k=v&` forms; JSON is emit-only):
//!   POST /api/polls      headers x-poll-id, x-admin-hash, x-mode, x-ttl;
//!                        body = question + options, one %-encoded value per line
//!   GET  /api/poll?id=   -> state; `counts` present only when !sealed || closed
//!   POST /api/vote       id=&choice=&voter=  -> insert or move this token's ballot
//!   POST /api/close      id=&admin=          -> close + reveal (idempotent)
//!   GET  /api/stream?id= -> SSE `tally` / `closed` events on topic <id>
//!   GET  /api/stats      -> counts only, never ids
//!   GET  / , /p/<id>     UI                  GET /ping   liveness

mod httpd;
mod sha256;

use httpd::{form_get, json, json_escape, url_decode, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 32 * 1024; // worst-case create: 13 %-encoded lines
const MAX_POLLS: usize = 5_000;
const MAX_VOTERS: usize = 50_000; // per poll
const MAX_QUESTION: usize = 280; // chars
const MAX_OPTION: usize = 120; // chars
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 12;
const MIN_TTL: u64 = 3600;
const MAX_TTL: u64 = 30 * 24 * 3600;
const DEFAULT_TTL: u64 = 7 * 24 * 3600;
const CLOSED_LINGER: u64 = 48 * 3600; // closed polls stay readable this long

struct Poll {
    question: String,
    options: Vec<String>,
    sealed: bool,
    counts: Vec<u64>,
    voters: HashMap<String, u8>, // sha256 hex of voter token -> choice
    admin_hash: String,          // sha256 hex of the creator's close token
    closed: bool,
    closed_at: Option<u64>,
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
    expires_at: u64,
}

struct App {
    polls: HashMap<String, Poll>,
    created: u64,    // lifetime counters, for the stats line
    votes_cast: u64, // accepted ballots, revotes included
    closed: u64,
    expired: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("ballot/0.1.0", 8080);
    let mut app = App {
        polls: HashMap::new(),
        created: 0,
        votes_cast: 0,
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
            let mut lapsed = 0u64;
            app.polls.retain(|_, p| match p.closed_at {
                // Closed polls linger so voters can come back for the result;
                // open polls just expire, tally never revealed.
                Some(c) => c.saturating_add(CLOSED_LINGER) > t,
                None => {
                    let keep = p.expires_at > t;
                    if !keep {
                        lapsed += 1;
                    }
                    keep
                }
            });
            app.expired += lapsed;
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.created + app.votes_cast, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                let ballots: usize = app.polls.values().map(|p| p.voters.len()).sum();
                println!(
                    "[ballot] holding {} polls / {} ballots; lifetime polls={} votes={} closed={} expired={}",
                    app.polls.len(),
                    ballots,
                    app.created,
                    app.votes_cast,
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
        ("GET", "/api/stats") => {
            let ballots: usize = app.polls.values().map(|p| p.voters.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"polls\":{},\"votes\":{},\"lifetime_polls\":{},\"lifetime_votes\":{}}}",
                    app.polls.len(),
                    ballots,
                    app.created,
                    app.votes_cast
                ),
            )
        }
        ("POST", "/api/polls") => api_create(app, req),
        ("GET", "/api/poll") => api_poll(app, req),
        ("POST", "/api/vote") => api_vote(app, srv, req),
        ("POST", "/api/close") => api_close(app, srv, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Poll ids are client-generated: ^[a-z0-9]{8,16}$.
fn valid_id(id: &str) -> bool {
    (8..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn api_create(app: &mut App, req: &Request) -> Response {
    let id = req.header("x-poll-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return json(400, "Bad Request", "{\"error\":\"bad id\"}".into());
    }
    let admin_hash = match req.header("x-admin-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return json(400, "Bad Request", "{\"error\":\"bad admin hash\"}".into()),
    };
    let sealed = match req.header("x-mode") {
        None | Some("sealed") => true,
        Some("live") => false,
        Some(_) => return json(400, "Bad Request", "{\"error\":\"bad mode\"}".into()),
    };
    let ttl: u64 = match req.header("x-ttl") {
        None => DEFAULT_TTL,
        Some(v) => match v.parse() {
            Ok(t) if (MIN_TTL..=MAX_TTL).contains(&t) => t,
            _ => return json(400, "Bad Request", "{\"error\":\"bad ttl\"}".into()),
        },
    };
    // Body: question then options, one value per line, each percent-encoded
    // so a newline inside a value can't smuggle in extra options.
    let Ok(text) = std::str::from_utf8(&req.body) else {
        return json(400, "Bad Request", "{\"error\":\"bad body\"}".into());
    };
    let mut lines = Vec::new();
    for line in text.lines() {
        match url_decode(line) {
            Some(v) if !v.trim().is_empty() => lines.push(v.trim().to_string()),
            _ => return json(400, "Bad Request", "{\"error\":\"bad line\"}".into()),
        }
    }
    if !(1 + MIN_OPTIONS..=1 + MAX_OPTIONS).contains(&lines.len()) {
        return json(400, "Bad Request", "{\"error\":\"need 2-12 options\"}".into());
    }
    let question = lines.remove(0);
    let options = lines;
    if question.chars().count() > MAX_QUESTION
        || options.iter().any(|o| o.chars().count() > MAX_OPTION)
    {
        return json(400, "Bad Request", "{\"error\":\"too long\"}".into());
    }
    if app.polls.len() >= MAX_POLLS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.polls.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    let expires_at = t + ttl;
    let n = options.len();
    app.created += 1;
    app.polls.insert(
        id,
        Poll {
            question,
            options,
            sealed,
            counts: vec![0; n],
            voters: HashMap::new(),
            admin_hash,
            closed: false,
            closed_at: None,
            created: t,
            expires_at,
        },
    );
    json(200, "OK", format!("{{\"ok\":true,\"expires_at\":{expires_at}}}"))
}

/// Look up a poll a client may still address: open and unexpired, or closed
/// and inside its linger window. Every other miss is the same 404.
fn live_poll(app: &mut App, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) {
        return Err(gone());
    }
    match app.polls.get(id) {
        Some(p) if p.closed || p.expires_at > now() => Ok(id.to_string()),
        Some(_) => {
            app.polls.remove(id);
            app.expired += 1;
            Err(gone())
        }
        None => Err(gone()),
    }
}

fn counts_json(counts: &[u64]) -> String {
    let inner = counts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

/// The one poll-state shape (/api/poll and the SSE initial event). The
/// `counts` key EXISTS only when the tally is public: live mode, or closed —
/// a sealed open poll doesn't emit zeros, it emits nothing. `your` is always
/// null: ballots are keyed by token hash and this server can't map one back
/// to a browser; the client remembers its own choice.
fn poll_json(p: &Poll) -> String {
    let opts = p
        .options
        .iter()
        .map(|o| format!("\"{}\"", json_escape(o)))
        .collect::<Vec<_>>()
        .join(",");
    let mut out = format!(
        "{{\"question\":\"{}\",\"options\":[{}],\"sealed\":{},\"closed\":{},\"total_votes\":{},\"expires_in\":{}",
        json_escape(&p.question),
        opts,
        p.sealed,
        p.closed,
        p.voters.len(),
        p.expires_at.saturating_sub(now())
    );
    if !p.sealed || p.closed {
        out.push_str(&format!(",\"counts\":{}", counts_json(&p.counts)));
    }
    out.push_str(",\"your\":null}");
    out
}

fn api_poll(app: &mut App, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_poll(app, &id) {
        Ok(id) => json(200, "OK", poll_json(&app.polls[&id])),
        Err(resp) => resp,
    }
}

fn api_vote(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_poll(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let voter = form_get(&body, "voter").unwrap_or_default();
    if !(16..=64).contains(&voter.len()) {
        return json(400, "Bad Request", "{\"error\":\"bad voter token\"}".into());
    }
    let p = app.polls.get_mut(&id).unwrap();
    if p.closed {
        return json(409, "Conflict", "{\"error\":\"closed\"}".into());
    }
    let choice = match form_get(&body, "choice").and_then(|c| c.parse::<usize>().ok()) {
        Some(c) if c < p.options.len() => c,
        _ => return json(400, "Bad Request", "{\"error\":\"bad choice\"}".into()),
    };
    // The ballot key is the hash of a token only the voter's browser knows;
    // the same token again moves the ballot, a new one is a new ballot.
    let ballot = sha256::hex(voter.as_bytes());
    if !p.voters.contains_key(&ballot) && p.voters.len() >= MAX_VOTERS {
        return json(507, "Insufficient Storage", "{\"error\":\"poll full\"}".into());
    }
    if let Some(old) = p.voters.insert(ballot, choice as u8) {
        p.counts[old as usize] -= 1; // revote: the old ballot leaves its pile
    }
    p.counts[choice] += 1;
    let total = p.voters.len();
    let sealed = p.sealed;
    let counts = counts_json(&p.counts);
    app.votes_cast += 1;
    // Everyone watching learns participation; only live mode shows the tally.
    let (event, resp) = if sealed {
        (
            format!("event: tally\ndata: {{\"total_votes\":{total}}}"),
            format!("{{\"ok\":true,\"total_votes\":{total}}}"),
        )
    } else {
        (
            format!("event: tally\ndata: {{\"total_votes\":{total},\"counts\":{counts}}}"),
            format!("{{\"ok\":true,\"total_votes\":{total},\"counts\":{counts}}}"),
        )
    };
    srv.broadcast(&id, &event);
    json(200, "OK", resp)
}

fn api_close(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_poll(app, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let admin = form_get(&body, "admin").unwrap_or_default();
    let p = app.polls.get_mut(&id).unwrap();
    if admin.is_empty() || sha256::hex(admin.as_bytes()) != p.admin_hash {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    let total = p.voters.len();
    let counts = counts_json(&p.counts);
    if !p.closed {
        p.closed = true;
        p.closed_at = Some(now());
        app.closed += 1;
        // The reveal: the only moment a sealed tally leaves enclave RAM.
        srv.broadcast(
            &id,
            &format!("event: closed\ndata: {{\"total_votes\":{total},\"counts\":{counts}}}"),
        );
    }
    json(200, "OK", format!("{{\"ok\":true,\"counts\":{counts}}}"))
}

/// SSE subscription for one poll. Late joiners get the current state as the
/// first (unnamed) event, framed exactly like /api/poll, so a reconnecting
/// client never needs a second fetch to be consistent.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_poll(app, &id) {
        Ok(id) => {
            let initial = format!("data: {}\n\n", poll_json(&app.polls[&id]));
            srv.upgrade_sse(key, &id, &initial);
        }
        Err(resp) => srv.respond(key, resp),
    }
}
