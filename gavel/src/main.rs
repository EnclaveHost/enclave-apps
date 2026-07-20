//! gavel — sealed-bid auctions the auctioneer can't rig.
//!
//! The Vickrey (second-price) auction is the mechanism economists actually
//! want: everyone's dominant strategy is to bid what the item is truly worth
//! to them. It's almost never used online, because it requires *absolute*
//! trust in the auctioneer — whoever can see the sealed bids can insert a
//! shill bid one unit under the winner and pocket the difference, and nobody
//! can ever prove they did. gavel closes that door by construction: bids sit
//! sealed in attested enclave RAM where nobody — the seller included — can
//! read them; the reserve price is committed (blinded) before the first bid
//! exists; the close is a pure function of the bid set; and the only things
//! that ever leave the enclave are the winner's name and the clearing price.
//! Losing bids are not revealed at close — they are *scrubbed*: amounts,
//! names, and contacts of losers are zeroed in RAM the moment the hammer
//! falls, so what was never revealed can no longer leak.
//!
//! Bidders are anonymous to the server the way ballot's voters are: a bid is
//! keyed by the SHA-256 of a random token the browser generated. The same
//! token re-bids in place (slot and priority stable — raising your bid keeps
//! your tie-break position; ties go to the earlier slot). What bidders can
//! verify at close: every bid's fingerprint sha256(token_hash ":" amount) is
//! published, so each bidder can recompute theirs and check their bid was in
//! the set the hammer considered; the reserve commitment sha256(salt ||
//! reserve) opens against the revealed salt. Correct *selection* of the max
//! rests on attestation — that's the part the TEE is for.
//!
//! Modes: `second` (winner pays max(second-highest bid, reserve) — their own
//! bid is never revealed, even to them being the winner changes nothing) and
//! `first` (winner pays their own bid; inherently revealed as the price).
//! With a single bid and no reserve the clearing price is 1 unit — set a
//! reserve if you care. An auction with no bid at or above the reserve
//! closes unsold; the reserve still opens, proving it was fixed all along.
//!
//! What the seller gets and when: nothing until close — /api/result answers
//! 409 while sealed, admin token or not. After close: winner's name, the
//! contact line the winner chose to attach, and the price. Logs carry
//! counts, never ids, names, amounts, or salts.
//!
//! API (bodies are `k=v&` forms, %-encoded; JSON is emit-only):
//!   POST /api/auctions   headers x-auction-id, x-admin-hash, x-mode,
//!                        x-deadline-in, x-reserve?, x-unit?; body title=&desc=
//!   GET  /api/auction?id=  -> state; reserve/salt/fingerprints only when closed
//!   POST /api/bid        id=&token=&amount=&name=&contact=?  (re-bid replaces)
//!   POST /api/close      id=&admin=   early hammer (idempotent)
//!   POST /api/cancel     id=&admin=   void it; bids erased, nothing revealed
//!   GET  /api/mine?id=&token=  -> your echo while open; won/lost after
//!   GET  /api/result?id=&admin= -> seller's view, only after close
//!   GET  /api/stream?id= -> SSE `bids` / `closed` / `cancelled` on topic <id>
//!   GET  /api/stats      -> counts only          GET / , /a/<id>  UI

mod httpd;
mod sha256;

use httpd::{form_get, json, json_escape, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const MAX_BODY: usize = 16 * 1024;
const MAX_AUCTIONS: usize = 2_000;
const MAX_BIDS: usize = 10_000; // per auction
const MAX_TITLE: usize = 80; // chars
const MAX_DESC: usize = 500; // chars
const MAX_NAME: usize = 40; // chars
const MAX_CONTACT: usize = 120; // chars
const MAX_UNIT: usize = 12; // chars
const MAX_AMOUNT: u64 = 9_999_999_999_999; // 13 digits keeps every sum exact in f64-land too
const MIN_DEADLINE: u64 = 60;
const MAX_DEADLINE: u64 = 30 * 24 * 3600;
const CLOSED_LINGER: u64 = 7 * 24 * 3600; // closed auctions stay checkable this long
const CANCEL_LINGER: u64 = 24 * 3600;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    First,
    Second,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::First => "first",
            Mode::Second => "second",
        }
    }
}

struct Bid {
    token_hash: String, // sha256 hex of the bidder's browser-held token
    amount: u64,        // scrubbed to 0 at close unless this slot won
    name: String,       // revealed only if this slot wins; scrubbed otherwise
    contact: String,    // shown only to the seller, only for the winner
}

struct Auction {
    title: String,
    desc: String,
    mode: Mode,
    unit: String,
    reserve: u64,   // 0 = no reserve; blinded by the commitment either way
    salt: [u8; 32], // the blind; revealed at close
    commit: String, // sha256(salt || reserve as ascii decimal), published at creation
    bids: Vec<Bid>, // insertion order = tie-break priority
    by_token: HashMap<String, usize>, // token hash -> slot
    admin_hash: String,
    deadline: u64,
    closed: bool,
    cancelled: bool,
    closed_at: Option<u64>,
    // Fixed at close, straight from the bid set:
    sold: bool,
    winner: Option<usize>,
    price: u64,
    fingerprints: Vec<String>, // sha256(token_hash ":" amount) per slot, close-time
    #[allow(dead_code)] // bookkeeping only; deliberately never exposed
    created: u64,
}

struct App {
    auctions: HashMap<String, Auction>,
    created: u64, // lifetime counters, for the stats line
    bids: u64,    // accepted bids, replacements included
    closed: u64,
    sold: u64,
    cancelled: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A 32-byte salt from the entropy std hands this target (see fairdraw for
/// the honest framing: each RandomState is freshly host-seeded, mixed with
/// the nanosecond clock through SHA-256). Here the salt only *blinds* the
/// reserve commitment — the auction's integrity doesn't rest on it.
fn fresh_salt() -> [u8; 32] {
    use std::hash::{BuildHasher, Hasher};
    let mut mix = Vec::with_capacity(200);
    for round in 0u8..8 {
        let state = std::collections::hash_map::RandomState::new();
        for input in [&b"gavel-salt"[..], b"sealed-bids", b"one-hammer"] {
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

/// The reserve commitment: sha256(salt bytes || reserve as ASCII decimal).
/// Published at creation whether or not a reserve exists, so bidders can't
/// tell — a secret reserve that provably predates every bid.
fn reserve_commit(salt: &[u8; 32], reserve: u64) -> String {
    let mut buf = Vec::with_capacity(32 + 20);
    buf.extend_from_slice(salt);
    buf.extend_from_slice(reserve.to_string().as_bytes());
    sha256::hex(&buf)
}

/// One bid's close-time fingerprint: sha256 over the UTF-8 of
/// "<token_hash_hex>:<amount decimal>". A bidder holds the token, so only
/// they can recompute theirs; amounts stay unbruteforceable to everyone
/// else because the token hash never leaves the enclave.
fn fingerprint(token_hash: &str, amount: u64) -> String {
    sha256::hex(format!("{token_hash}:{amount}").as_bytes())
}

/// The hammer, as a pure function of the bid set. Winner = highest amount,
/// ties to the earlier slot. Sold iff that amount clears the reserve.
/// Price: first-price pays the winning bid; second-price pays
/// max(second-highest amount, reserve, 1).
fn settle(bids: &[Bid], mode: Mode, reserve: u64) -> (bool, Option<usize>, u64) {
    let Some((win, _)) = bids
        .iter()
        .enumerate()
        .max_by(|(ai, a), (bi, b)| a.amount.cmp(&b.amount).then(bi.cmp(ai)))
    else {
        return (false, None, 0);
    };
    let high = bids[win].amount;
    if high < reserve {
        return (false, None, 0);
    }
    let price = match mode {
        Mode::First => high,
        Mode::Second => {
            let second = bids
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != win)
                .map(|(_, b)| b.amount)
                .max()
                .unwrap_or(0);
            second.max(reserve).max(1)
        }
    };
    (true, Some(win), price)
}

fn main() {
    let mut srv = Server::bind("gavel/0.1.0", 8080);
    let mut app = App {
        auctions: HashMap::new(),
        created: 0,
        bids: 0,
        closed: 0,
        sold: 0,
        cancelled: 0,
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
            // Deadlines: an open auction past its close goes through the
            // same hammer a manual close takes.
            let due: Vec<String> = app
                .auctions
                .iter()
                .filter(|(_, a)| !a.closed && !a.cancelled && a.deadline <= t)
                .map(|(id, _)| id.clone())
                .collect();
            for id in due {
                close_auction(&mut app, &mut srv, &id);
            }
            app.auctions.retain(|_, a| match a.closed_at {
                Some(c) if a.cancelled => c.saturating_add(CANCEL_LINGER) > t,
                Some(c) => c.saturating_add(CLOSED_LINGER) > t,
                None => true, // open auctions always have a deadline ahead
            });
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.created + app.bids, t / 600);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                let bids: usize = app.auctions.values().map(|a| a.bids.len()).sum();
                println!(
                    "[gavel] holding {} auctions / {} sealed bids; lifetime auctions={} bids={} closed={} sold={} cancelled={}",
                    app.auctions.len(),
                    bids,
                    app.created,
                    app.bids,
                    app.closed,
                    app.sold,
                    app.cancelled
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let resp = match (req.method.as_str(), req.path.as_str()) {
        // One embedded page for / and /a/<id>; the client routes on pathname.
        ("GET", p) if p == "/" || p == "/index.html" || p.starts_with("/a/") => {
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
            let bids: usize = app.auctions.values().map(|a| a.bids.len()).sum();
            json(
                200,
                "OK",
                format!(
                    "{{\"auctions\":{},\"sealed_bids\":{},\"lifetime_auctions\":{},\"lifetime_bids\":{},\"closed\":{},\"sold\":{}}}",
                    app.auctions.len(),
                    bids,
                    app.created,
                    app.bids,
                    app.closed,
                    app.sold
                ),
            )
        }
        ("POST", "/api/auctions") => api_create(app, req),
        ("GET", "/api/auction") => api_auction(app, srv, req),
        ("POST", "/api/bid") => api_bid(app, srv, req),
        ("POST", "/api/close") => api_close(app, srv, req),
        ("POST", "/api/cancel") => api_cancel(app, srv, req),
        ("GET", "/api/mine") => api_mine(app, srv, req),
        ("GET", "/api/result") => api_result(app, srv, req),
        ("GET", "/api/stream") => return api_stream(app, srv, key, req),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    };
    srv.respond(key, resp);
}

/// Auction ids are client-generated: ^[a-z0-9]{8,16}$.
fn valid_id(id: &str) -> bool {
    (8..=16).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Amounts arrive as ASCII decimal form fields; parse strictly.
fn parse_amount(s: &str, min: u64) -> Option<u64> {
    if s.is_empty() || s.len() > 13 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u64 = s.parse().ok()?;
    (min..=MAX_AMOUNT).contains(&v).then_some(v)
}

fn clean_text(s: &str, max_chars: usize) -> Option<String> {
    let s = s.trim();
    (s.chars().count() <= max_chars && !s.chars().any(|c| (c as u32) < 0x20 && c != '\n'))
        .then(|| s.to_string())
}

fn api_create(app: &mut App, req: &Request) -> Response {
    let bad = |what: &str| json(400, "Bad Request", format!("{{\"error\":\"bad {what}\"}}"));
    let id = req.header("x-auction-id").unwrap_or("").to_string();
    if !valid_id(&id) {
        return bad("id");
    }
    let admin_hash = match req.header("x-admin-hash") {
        Some(h) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
            h.to_ascii_lowercase()
        }
        _ => return bad("admin hash"),
    };
    let mode = match req.header("x-mode") {
        Some("first") => Mode::First,
        Some("second") | None => Mode::Second,
        _ => return bad("mode"),
    };
    let deadline_in: u64 = match req.header("x-deadline-in").and_then(|v| v.parse().ok()) {
        Some(s) if (MIN_DEADLINE..=MAX_DEADLINE).contains(&s) => s,
        _ => return bad("deadline"),
    };
    let reserve = match req.header("x-reserve") {
        None => 0,
        Some(v) => match parse_amount(v, 0) {
            Some(r) => r,
            None => return bad("reserve"),
        },
    };
    let unit = match req.header("x-unit") {
        None => String::new(),
        Some(u) => match clean_text(u, MAX_UNIT) {
            Some(u) if !u.contains('\n') => u,
            _ => return bad("unit"),
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
    if app.auctions.len() >= MAX_AUCTIONS {
        return json(
            507,
            "Insufficient Storage",
            "{\"error\":\"at capacity, try later\"}".into(),
        );
    }
    if app.auctions.contains_key(&id) {
        return json(409, "Conflict", "{\"error\":\"exists\"}".into());
    }
    let t = now();
    // The whole trick: the reserve is fixed and blinded before the first
    // bid can exist, and bids will only ever be readable by the hammer.
    let salt = fresh_salt();
    let commit = reserve_commit(&salt, reserve);
    app.created += 1;
    app.auctions.insert(
        id,
        Auction {
            title,
            desc,
            mode,
            unit,
            reserve,
            salt,
            commit: commit.clone(),
            bids: Vec::new(),
            by_token: HashMap::new(),
            admin_hash,
            deadline: t + deadline_in,
            closed: false,
            cancelled: false,
            closed_at: None,
            sold: false,
            winner: None,
            price: 0,
            fingerprints: Vec::new(),
            created: t,
        },
    );
    json(
        200,
        "OK",
        format!(
            "{{\"ok\":true,\"commit\":\"{commit}\",\"closes_at\":{}}}",
            t + deadline_in
        ),
    )
}

/// Resolve an auction a client may still address, closing it first if its
/// deadline passed between sweeps — so no bid ever lands after the hammer.
/// Every other miss is the same 404.
fn live_auction(app: &mut App, srv: &mut Server, id: &str) -> Result<String, Response> {
    let gone = || json(404, "Not Found", "{\"error\":\"gone\"}".into());
    if !valid_id(id) || !app.auctions.contains_key(id) {
        return Err(gone());
    }
    let overdue = {
        let a = &app.auctions[id];
        !a.closed && !a.cancelled && a.deadline <= now()
    };
    if overdue {
        close_auction(app, srv, id);
    }
    Ok(id.to_string())
}

/// The one auction-state shape (/api/auction, the SSE initial event, the
/// closed broadcast). The keys that decide the result — `reserve`, `salt`,
/// `fingerprints`, `winner`, `price` — EXIST only once the hammer has
/// fallen: that asymmetry is the entire app.
fn auction_json(a: &Auction) -> String {
    let t = now();
    let expires_in = match a.closed_at {
        Some(c) if a.cancelled => c.saturating_add(CANCEL_LINGER).saturating_sub(t),
        Some(c) => c.saturating_add(CLOSED_LINGER).saturating_sub(t),
        None => a.deadline.saturating_sub(t),
    };
    let mut out = format!(
        "{{\"title\":\"{}\",\"desc\":\"{}\",\"mode\":\"{}\",\"unit\":\"{}\",\"commit\":\"{}\",\"count\":{},\"closed\":{},\"cancelled\":{},\"closes_in\":{},\"expires_in\":{}",
        json_escape(&a.title),
        json_escape(&a.desc),
        a.mode.as_str(),
        json_escape(&a.unit),
        a.commit,
        a.bids.len(),
        a.closed,
        a.cancelled,
        if a.closed || a.cancelled {
            "null".into()
        } else {
            a.deadline.saturating_sub(t).to_string()
        },
        expires_in
    );
    if a.closed {
        let winner = match a.winner {
            Some(i) => format!(
                "{{\"i\":{},\"name\":\"{}\"}}",
                i,
                json_escape(&a.bids[i].name)
            ),
            None => "null".into(),
        };
        let prints = a
            .fingerprints
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            ",\"sold\":{},\"price\":{},\"winner\":{},\"reserve\":{},\"salt\":\"{}\",\"fingerprints\":[{}]",
            a.sold,
            if a.sold { a.price.to_string() } else { "null".into() },
            winner,
            a.reserve,
            to_hex(&a.salt),
            prints
        ));
    }
    out.push('}');
    out
}

fn api_auction(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_auction(app, srv, &id) {
        Ok(id) => json(200, "OK", auction_json(&app.auctions[&id])),
        Err(resp) => resp,
    }
}

fn api_bid(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_auction(app, srv, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if app.auctions[&id].closed || app.auctions[&id].cancelled {
        return json(409, "Conflict", "{\"error\":\"closed\"}".into());
    }
    let token = form_get(&body, "token").unwrap_or_default();
    if !(16..=64).contains(&token.len()) {
        return json(400, "Bad Request", "{\"error\":\"bad token\"}".into());
    }
    let amount = match parse_amount(&form_get(&body, "amount").unwrap_or_default(), 1) {
        Some(v) => v,
        None => return json(400, "Bad Request", "{\"error\":\"bad amount\"}".into()),
    };
    let name = match form_get(&body, "name").and_then(|n| clean_text(&n, MAX_NAME)) {
        Some(n) if !n.is_empty() && !n.contains('\n') => n,
        _ => return json(400, "Bad Request", "{\"error\":\"bad name\"}".into()),
    };
    let contact = match form_get(&body, "contact") {
        None => String::new(),
        Some(c) => match clean_text(&c, MAX_CONTACT) {
            Some(c) if !c.contains('\n') => c,
            _ => return json(400, "Bad Request", "{\"error\":\"bad contact\"}".into()),
        },
    };
    // The bid's key is the hash of a token only the bidder's browser knows;
    // the same token re-bids its own slot — priority stable, so raising
    // your bid keeps your tie-break position — and a new one appends.
    let token_hash = sha256::hex(token.as_bytes());
    let a = app.auctions.get_mut(&id).unwrap();
    match a.by_token.get(&token_hash) {
        Some(&i) => {
            let b = &mut a.bids[i];
            b.amount = amount;
            b.name = name;
            b.contact = contact;
        }
        None => {
            if a.bids.len() >= MAX_BIDS {
                return json(507, "Insufficient Storage", "{\"error\":\"auction full\"}".into());
            }
            a.by_token.insert(token_hash.clone(), a.bids.len());
            a.bids.push(Bid {
                token_hash,
                amount,
                name,
                contact,
            });
        }
    }
    let count = a.bids.len();
    app.bids += 1;
    // Everyone watching learns participation, never amounts.
    srv.broadcast(&id, &format!("event: bids\ndata: {{\"count\":{count}}}"));
    json(200, "OK", format!("{{\"ok\":true,\"count\":{count}}}"))
}

/// The one hammer path — /api/close, the deadline sweep, and a too-late bid
/// all land here. Idempotent. This is the only moment amounts are read, and
/// the moment every losing bid is scrubbed from RAM.
fn close_auction(app: &mut App, srv: &mut Server, id: &str) {
    let Some(a) = app.auctions.get_mut(id) else { return };
    if a.closed || a.cancelled {
        return;
    }
    let (sold, winner, price) = settle(&a.bids, a.mode, a.reserve);
    a.fingerprints = a
        .bids
        .iter()
        .map(|b| fingerprint(&b.token_hash, b.amount))
        .collect();
    // Scrub what the reveal doesn't need: losing amounts, names, contacts
    // cease to exist the moment they've been weighed. The winner keeps name
    // (public) and contact (for the seller); the winning amount survives
    // only in first-price mode, where it IS the price.
    for (i, b) in a.bids.iter_mut().enumerate() {
        if Some(i) != winner {
            b.amount = 0;
            b.name.clear();
            b.contact.clear();
        }
    }
    if let (Some(w), Mode::Second) = (winner, a.mode) {
        a.bids[w].amount = 0;
    }
    a.sold = sold;
    a.winner = winner;
    a.price = price;
    a.closed = true;
    a.closed_at = Some(now());
    app.closed += 1;
    if sold {
        app.sold += 1;
    }
    let payload = auction_json(&app.auctions[id]);
    srv.broadcast(id, &format!("event: closed\ndata: {payload}"));
}

fn check_admin(app: &App, id: &str, admin: &str) -> bool {
    !admin.is_empty() && sha256::hex(admin.as_bytes()) == app.auctions[id].admin_hash
}

fn api_close(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_auction(app, srv, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if !check_admin(app, &id, &form_get(&body, "admin").unwrap_or_default()) {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    if app.auctions[&id].cancelled {
        return json(409, "Conflict", "{\"error\":\"cancelled\"}".into());
    }
    close_auction(app, srv, &id);
    json(200, "OK", auction_json(&app.auctions[&id]))
}

/// Cancel voids an open auction: every sealed bid is erased on the spot and
/// nothing — reserve included — is ever revealed.
fn api_cancel(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let id = match live_auction(app, srv, &form_get(&body, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if !check_admin(app, &id, &form_get(&body, "admin").unwrap_or_default()) {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    if app.auctions[&id].closed {
        return json(409, "Conflict", "{\"error\":\"closed\"}".into());
    }
    let a = app.auctions.get_mut(&id).unwrap();
    if !a.cancelled {
        a.bids.clear();
        a.by_token.clear();
        a.cancelled = true;
        a.closed_at = Some(now());
        app.cancelled += 1;
        srv.broadcast(&id, "event: cancelled\ndata: {}");
    }
    json(200, "OK", auction_json(&app.auctions[&id]))
}

/// A bidder's private echo, keyed by their token. Open: your recorded
/// amount, so you can see the seal took. Closed: won (with what you owe) or
/// a uniform lost — no rank, no distance, nothing to reverse-engineer.
fn api_mine(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let id = match live_auction(app, srv, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let token = form_get(&req.query, "token").unwrap_or_default();
    let token_hash = sha256::hex(token.as_bytes());
    let a = &app.auctions[&id];
    if a.cancelled {
        return json(200, "OK", "{\"cancelled\":true}".into());
    }
    if !a.closed {
        let bid = a
            .by_token
            .get(&token_hash)
            .map(|&i| a.bids[i].amount.to_string())
            .unwrap_or("null".into());
        return json(200, "OK", format!("{{\"closed\":false,\"bid\":{bid}}}"));
    }
    let won = a.sold && a.winner.is_some_and(|w| a.bids[w].token_hash == token_hash);
    if won {
        json(
            200,
            "OK",
            format!("{{\"closed\":true,\"won\":true,\"pay\":{}}}", a.price),
        )
    } else {
        json(200, "OK", "{\"closed\":true,\"won\":false}".into())
    }
}

/// The seller's view — and the proof the seller has no other one: with the
/// admin token in hand, this answers 409 until the hammer falls.
fn api_result(app: &mut App, srv: &mut Server, req: &Request) -> Response {
    let id = match live_auction(app, srv, &form_get(&req.query, "id").unwrap_or_default()) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    if !check_admin(app, &id, &form_get(&req.query, "admin").unwrap_or_default()) {
        return json(403, "Forbidden", "{\"error\":\"bad token\"}".into());
    }
    let a = &app.auctions[&id];
    if a.cancelled {
        return json(200, "OK", "{\"cancelled\":true}".into());
    }
    if !a.closed {
        return json(409, "Conflict", "{\"error\":\"still sealed\"}".into());
    }
    match a.winner.filter(|_| a.sold) {
        Some(w) => json(
            200,
            "OK",
            format!(
                "{{\"sold\":true,\"price\":{},\"count\":{},\"winner\":\"{}\",\"contact\":\"{}\"}}",
                a.price,
                a.bids.len(),
                json_escape(&a.bids[w].name),
                json_escape(&a.bids[w].contact)
            ),
        ),
        None => json(
            200,
            "OK",
            format!("{{\"sold\":false,\"count\":{}}}", a.bids.len()),
        ),
    }
}

/// SSE subscription for one auction. Late joiners get the current state as
/// the first (unnamed) event, framed exactly like /api/auction.
fn api_stream(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let id = form_get(&req.query, "id").unwrap_or_default();
    match live_auction(app, srv, &id) {
        Ok(id) => {
            let initial = format!("data: {}\n\n", auction_json(&app.auctions[&id]));
            srv.upgrade_sse(key, &id, &initial);
        }
        Err(resp) => srv.respond(key, resp),
    }
}
