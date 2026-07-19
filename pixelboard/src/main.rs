//! pixelboard — a collaborative pixel canvas as an Enclave service app.
//!
//! One shared 128×128 board, sixteen colors, one placement per session
//! every two seconds. The whole artwork is 16 KiB of palette indices in
//! enclave RAM: while the deployment is funded it belongs to everyone
//! painting on it, and when the funding ends it provably ceases to exist.
//! Running inside an attested TEE is what makes the rules mean something:
//! the cooldown and the canvas are reproducible code plus memory nobody can
//! reach — no faster brushes, no out-of-band edits, no disk for a snapshot
//! to leak onto.
//!
//! The server never learns who paints. A session is an opaque random token
//! the browser mints for itself, held only to meter the cooldown and swept
//! a minute after the last placement. Logs carry counts, never tokens.
//!
//! API (bodies are `k=v&` forms; JSON is emit-only):
//!   GET  /api/board            -> {w,h,palette,placed,version,painting,board:b64}
//!   POST /api/px  i=&c=&s=     -> {ok,wait_ms} · on cooldown 429 {wait_ms}
//!   GET  /api/stream           -> SSE: "px" delta batches, "n" live counts
//!   GET  /api/stats            -> counts only, never tokens
//!   GET  /            UI        GET /ping    liveness

mod httpd;

use httpd::{form_get, json, Request, Response, Server};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const UI: &str = include_str!("ui.html");

const W: usize = 128;
const H: usize = 128;
const CELLS: usize = W * H;
const COLORS: u8 = 16;
const COOLDOWN_MS: u64 = 2000;
const SESSION_TTL_MS: u64 = 60_000; // forget a hand a minute after its last pixel
const FLUSH_MS: u64 = 200; // at most one "px" frame per flush window
const NOTE_MS: u64 = 5000; // "n" (who's painting) cadence
const SWEEP_MS: u64 = 30_000;
const MAX_BODY: usize = 4 * 1024;

/// Fixed sixteen, served to the client with the board. Index 0 is the
/// background — the suite's --bg — so a fresh board reads as empty space,
/// not a wall of paint.
const PALETTE: [&str; 16] = [
    "#0b0e14", "#ffffff", "#c8cdd8", "#8a94a6", "#e05e5e", "#e8a34c",
    "#f2d54f", "#6fcf8f", "#39b3a6", "#4cc9e8", "#5a8bf5", "#a78bfa",
    "#e07ad0", "#8a5a3c", "#3a4256", "#141a26",
];

struct App {
    board: Vec<u8>,                 // CELLS palette indices — the artwork itself
    sessions: HashMap<String, u64>, // token -> last placement (unix millis)
    pending: Vec<(u16, u8)>,        // deltas since the last SSE flush
    placed: u64,                    // lifetime placements, for the stats line
    version: u64,                   // bumps per placement; clients detect gaps
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn main() {
    let mut srv = Server::bind("pixelboard/0.1.0", 8080);
    let mut app = App {
        board: vec![0u8; CELLS],
        sessions: HashMap::new(),
        pending: Vec::new(),
        placed: 0,
        version: 0,
    };
    let (mut last_flush, mut last_note, mut last_sweep) = (0u64, 0u64, 0u64);
    let mut last_stat = (0u64, 0u64);
    loop {
        for (key, req) in srv.poll(MAX_BODY) {
            if req.method == "GET" && req.path == "/api/stream" {
                srv.upgrade_sse(key, "board", "");
                continue;
            }
            let painting = srv.sse_count("board");
            let resp = route(&mut app, &req, painting);
            srv.respond(key, resp);
        }
        let t = now_ms();
        // Batched delta flush: one frame per window however fast people
        // paint; v after the batch lets clients spot a gap and refetch.
        if !app.pending.is_empty() && t.saturating_sub(last_flush) >= FLUSH_MS {
            last_flush = t;
            let mut d = String::with_capacity(app.pending.len() * 10 + 2);
            for (n, (i, c)) in app.pending.iter().enumerate() {
                if n > 0 {
                    d.push(',');
                }
                d.push_str(&format!("[{i},{c}]"));
            }
            let ev = format!("event: px\ndata: {{\"v\":{},\"d\":[{d}]}}", app.version);
            srv.broadcast("board", &ev);
            app.pending.clear();
        }
        if t.saturating_sub(last_note) >= NOTE_MS {
            last_note = t;
            let ev = format!(
                "event: n\ndata: {{\"painting\":{},\"placed\":{}}}",
                srv.sse_count("board"),
                app.placed
            );
            srv.broadcast("board", &ev);
        }
        if t.saturating_sub(last_sweep) >= SWEEP_MS {
            last_sweep = t;
            app.sessions
                .retain(|_, last| t.saturating_sub(*last) <= SESSION_TTL_MS);
            // A counts-only heartbeat at most every 10 minutes, only on change.
            let stat = (app.placed, t / 600_000);
            if stat.1 != last_stat.1 && stat.0 != last_stat.0 {
                last_stat = stat;
                println!(
                    "[pixelboard] board v{}; lifetime placed={}; {} sessions warm, {} painting now",
                    app.version,
                    app.placed,
                    app.sessions.len(),
                    srv.sse_count("board")
                );
            }
        }
        srv.flush_and_sleep();
    }
}

fn route(app: &mut App, req: &Request, painting: usize) -> Response {
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
        ("GET", "/api/board") => json(
            200,
            "OK",
            format!(
                "{{\"w\":{W},\"h\":{H},\"palette\":[{}],\"placed\":{},\"version\":{},\"painting\":{painting},\"board\":\"{}\"}}",
                palette_json(),
                app.placed,
                app.version,
                base64(&app.board)
            ),
        ),
        ("POST", "/api/px") => api_px(app, req),
        ("GET", "/api/stats") => json(
            200,
            "OK",
            format!(
                "{{\"placed\":{},\"painting\":{painting},\"sessions\":{}}}",
                app.placed,
                app.sessions.len()
            ),
        ),
        _ => json(404, "Not Found", "{\"error\":\"gone\"}".into()),
    }
}

/// A session token is whatever opaque random string the browser minted for
/// itself — the server never issues one, so the most it ever knows is
/// "some hand painted two seconds ago".
fn valid_session(s: &str) -> bool {
    (16..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn api_px(app: &mut App, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let bad = || json(400, "Bad Request", "{\"error\":\"bad params\"}".into());
    let Some(i) = form_get(&body, "i").and_then(|v| v.parse::<usize>().ok()) else {
        return bad();
    };
    let Some(c) = form_get(&body, "c").and_then(|v| v.parse::<u8>().ok()) else {
        return bad();
    };
    let s = form_get(&body, "s").unwrap_or_default();
    if i >= CELLS || c >= COLORS || !valid_session(&s) {
        return bad();
    }
    let t = now_ms();
    // Check-cooldown-paint-stamp is one uninterruptible step — wasip2 has
    // no threads — so the 2-second rule cannot be raced, by construction.
    if let Some(last) = app.sessions.get(&s) {
        let since = t.saturating_sub(*last);
        if since < COOLDOWN_MS {
            return json(
                429,
                "Too Many Requests",
                format!("{{\"wait_ms\":{}}}", COOLDOWN_MS - since),
            );
        }
    }
    app.sessions.insert(s, t);
    app.board[i] = c;
    app.pending.push((i as u16, c));
    app.placed += 1;
    app.version += 1;
    json(200, "OK", format!("{{\"ok\":true,\"wait_ms\":{COOLDOWN_MS}}}"))
}

fn palette_json() -> String {
    let mut out = String::with_capacity(PALETTE.len() * 10);
    for (i, c) in PALETTE.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(c);
        out.push('"');
    }
    out
}

/// Standard-alphabet base64 with padding, hand-rolled like everything else
/// here — the board is 16 KiB of raw indices and this is the whole encoder.
fn base64(data: &[u8]) -> String {
    const AB: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(AB[(n >> 18 & 63) as usize] as char);
        out.push(AB[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { AB[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { AB[(n & 63) as usize] as char } else { '=' });
    }
    out
}
