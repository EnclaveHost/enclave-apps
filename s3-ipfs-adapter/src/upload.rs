//! The pin routes: /add-wasm, /add-json, /add-image — the Rust port of the
//! nan box's validating upload gateway (scripts/ipfs-add-gateway.py), moved
//! inside the enclave so ipfs.enclave.host can be served entirely by this
//! app. Wire compatibility is the contract: same routes, same
//! x-upload-address / x-upload-expiry / x-upload-token wallet-HMAC auth
//! for legacy clients, plus deployment-bound EIP-191 signatures, same response shapes
//! ({"cid", ...}), same refusal codes (401/403/413/415/429), same CORS echo.
//!
//! The opt-in /add-file route stores generic runtime artifacts with the same
//! bounded multipart path, but requires direct wallet authorization. It does
//! not classify bytes as WASM or change runtime admission.
//!
//! What replaces Kubo: bytes are PUT into the configured S3 bucket under
//! `pins/<cid>`, where the indexer's snapshot serves them right back over
//! /ipfs/<cid> with a CID computed by the same code that computed it at
//! upload time. Small bodies (json/image, and wasm up to one part) are
//! buffered and written once; larger wasm streams through an S3 multipart
//! upload on a staging key as the body arrives — a 2 GiB component never
//! lives in this guest's memory — then a server-side copy renames it to its
//! CID once the whole body has been verified.
//!
//! Order of operations on the streaming path, deliberately: token-shape
//! checks refuse before any byte is accepted; the wasm preamble refuses at
//! byte 8; the HMAC (which binds the token to sha256(body)) can only be
//! checked after the last byte, so a hostile stream costs bounded S3 churn
//! on a staging key that is aborted, never a served object.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::httpd::{json, json_escape, Request, Response, Server, Sink};
use crate::ipfs::{self, Cid, CHUNK};
use crate::s3;
use crate::wasmscan::WasmScan;
use crate::{App, S3Ctx};

/// One multipart part: 32 whole IPFS chunks. All parts except the last must
/// be the same size (R2 enforces uniformity), and a part boundary must land
/// on a chunk boundary so leaf hashing never straddles a flush.
pub const PART_SIZE: usize = 32 * CHUNK as usize; // 8 MiB

/// Object-key prefix (under the configured bucket prefix) for pinned bytes.
pub const PIN_PREFIX: &str = "pins/";
const STAGING_PREFIX: &str = "staging/";

/// At most this many concurrent streaming uploads; refuse past it.
const UPLOAD_SLOTS: u32 = 4;

pub struct UploadCfg {
    pub upload_key: String, // shared HMAC secret with the api-relay
    pub wallet_audience: String, // full deployment ID, signed by direct wallet clients
    pub allow_unsigned: bool, // dev/e2e only: no key, uploads open
    pub allow_origins: Vec<String>,
    pub max_wasm: u64,
    pub max_file: u64, // explicit opt-in; zero disables generic runtime/artifact uploads
    pub max_image: usize,
    pub max_json: usize,
    pub per_addr_daily: u64,
    pub global_daily: u64,
    pub json_per_ip_hourly: f64,
    /// `contractWallets`: the chain and pinned readers for smart-account (ERC-1271) uploads; None = v2 refused
    pub contract_wallets: Option<crate::account_upload::AccountCfg>,
}

impl UploadCfg {
    pub fn enabled(&self) -> bool {
        !self.upload_key.is_empty() || crate::wallet_upload::canonical_hex(&self.wallet_audience, 32, true) || self.allow_unsigned
    }
}

/// State shared between the route handlers, in-flight sinks and the main
/// loop. In-memory daily byte counters, reset at UTC midnight — a process
/// restart resets them, which only an operator (or a redeploy) can trigger,
/// an adequate abuse bound without a datastore.
pub struct Shared {
    day: u64, // days since epoch, UTC
    global: u64,
    addr: HashMap<String, u64>,
    json_rl: HashMap<String, (f64, f64)>, // ip -> (tokens, last)
    pub slots: u32,
    seq: u64,
    /// Uploads that completed in S3 and await the snapshot merge the main
    /// loop performs (it owns &mut App; sinks do not).
    pub commits: Vec<PendingCommit>,
    /// smart-account uploads: used (account, nonce) pairs and the chain-check budget
    pub account_guard: crate::account_upload::Guard,
}

pub struct PendingCommit {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub leaves: Vec<[u8; 32]>,
}

impl Shared {
    pub fn new() -> Rc<RefCell<Shared>> {
        Rc::new(RefCell::new(Shared {
            day: 0,
            global: 0,
            addr: HashMap::new(),
            json_rl: HashMap::new(),
            slots: 0,
            seq: 0,
            commits: Vec::new(),
            account_guard: Default::default(),
        }))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn roll_day(sh: &mut Shared) {
    let day = now_secs() / 86400;
    if sh.day != day {
        sh.day = day;
        sh.global = 0;
        sh.addr.clear();
    }
}

/// Reserve nbytes against the FLEET (global) daily cap only. Keyed on nothing
/// but the total, so it cannot be used to grief a specific wallet's budget -
/// which is why the streaming route reserves this at header time (before the
/// wallet is verified) and refunds it on any failure.
fn reserve_global(sh: &mut Shared, global_daily: u64, n: u64) -> Result<(), String> {
    roll_day(sh);
    if sh.global + n > global_daily {
        return Err("fleet daily upload limit reached; retry tomorrow".into());
    }
    sh.global += n;
    Ok(())
}

fn release_global(sh: &mut Shared, n: u64) {
    sh.global = sh.global.saturating_sub(n);
}

/// Reserve against one wallet's daily cap. Only ever called post-auth (a
/// verified address), so it cannot be griefed by a forged X-Upload-Address.
fn reserve_addr(sh: &mut Shared, per_addr_daily: u64, address: &str, n: u64) -> Result<(), String> {
    roll_day(sh);
    let used = sh.addr.get(address).copied().unwrap_or(0);
    if used + n > per_addr_daily {
        return Err("this wallet's daily upload limit reached; retry tomorrow".into());
    }
    sh.addr.insert(address.to_string(), used + n);
    Ok(())
}

/// Reserve nbytes against the per-wallet + global daily caps atomically (the
/// buffered routes, where the wallet is already verified).
fn reserve_bytes(
    sh: &mut Shared,
    per_addr_daily: u64,
    global_daily: u64,
    address: &str,
    n: u64,
) -> Result<(), String> {
    reserve_global(sh, global_daily, n)?;
    if let Err(e) = reserve_addr(sh, per_addr_daily, address, n) {
        release_global(sh, n); // roll back the global half
        return Err(e);
    }
    Ok(())
}

/// The interim per-IP bucket in front of /add-json's HMAC work: an unsigned
/// flood costs no crypto. Keyed on the LAST X-Forwarded-For entry — the one
/// the proxy in front of this app appended — never the first, which is
/// whatever the sender typed (same reasoning as api-relay's clientIp).
fn json_rate_ok(sh: &mut Shared, cfg: &UploadCfg, req: &Request) -> bool {
    let ip = req
        .header("x-forwarded-for")
        .and_then(|v| v.split(',').next_back())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let now = now_secs() as f64;
    let cap = cfg.json_per_ip_hourly;
    let refill = cap / 3600.0;
    let (tok, last) = sh.json_rl.get(&ip).copied().unwrap_or((cap, now));
    let tok = cap.min(tok + (now - last) * refill);
    if tok < 1.0 {
        sh.json_rl.insert(ip, (tok, now));
        return false;
    }
    sh.json_rl.insert(ip, (tok - 1.0, now));
    true
}

// ---- wallet-HMAC auth -------------------------------------------------------

pub struct AuthHdrs {
    address: String,
    expiry: u64,
    token: String,
    wallet_body: Option<(String, u64)>,
}

/// Header-shape gate, decidable before any body byte: presence, address and
/// expiry formats, freshness. Returns Ok(None) when auth is disabled
/// (allow_unsigned dev mode). The HMAC itself binds to sha256(body), so it
/// is checked in `verify_body` once the body is complete. Direct signatures
/// are checked here, and their declared body hash is checked at completion.
fn precheck(cfg: &UploadCfg, req: &Request) -> Result<Option<AuthHdrs>, (u16, String)> {
    precheck_with(cfg, req, None)
}

/// precheck, plus smart-account (ERC-1271) uploads when the route passes the shared state: their replay memory and
/// chain-check budget live there, and the account's contract is asked through the operator's pinned readers
/// (account_upload.rs) before a single body byte is accepted.
fn precheck_with(cfg: &UploadCfg, req: &Request, shared: Option<&RefCell<Shared>>) -> Result<Option<AuthHdrs>, (u16, String)> {
    for name in ["x-upload-address", "x-upload-expiry", "x-upload-token", "x-upload-signature", "x-upload-sha256"] {
        if req.headers.iter().filter(|(k,_)| k == name).count() > 1 {
            return Err((401, "duplicate upload authorization header".into()));
        }
    }
    // an explicitly declared smart-account upload is decided ONLY by the account (never falls back to another scheme)
    if req.header("x-upload-scheme").is_some() {
        let claim = crate::account_upload::parse(cfg.contract_wallets.as_ref(), &cfg.wallet_audience, req, now_secs())?;
        let (Some(acfg), Some(sh)) = (cfg.contract_wallets.as_ref(), shared) else {
            return Err((401, "smart-account uploads are not available on this route".into()));
        };
        crate::account_upload::verify(acfg, &claim, &mut sh.borrow_mut().account_guard, &mut crate::account_upload::NetReaders)?;
        return Ok(Some(AuthHdrs { address: claim.account, expiry: claim.expiry, token: String::new(), wallet_body: Some((claim.hash, claim.size)) }));
    }
    let wallet_attempt = req.header("x-upload-signature").is_some() || req.header("x-upload-sha256").is_some();
    // The generic route accepts only deployment- and path-bound signatures.
    // Legacy HMAC tokens have no path binding and cannot authorize this route.
    if req.path == "/add-file" && !wallet_attempt {
        return Err((401, "file uploads require a direct wallet signature".into()));
    }
    if !wallet_attempt && cfg.upload_key.is_empty() && cfg.allow_unsigned {
        return Ok(None);
    }
    let address = req
        .header("x-upload-address")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let expiry = req.header("x-upload-expiry").unwrap_or("").trim().to_string();
    let token = req
        .header("x-upload-token")
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if address.is_empty() || expiry.is_empty() || (!wallet_attempt && token.is_empty()) {
        return Err((401, "signed upload required: connect your wallet and retry (the console/CLI signs the upload)".into()));
    }
    if !(address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err((401, "bad upload address".into()));
    }
    let Ok(exp) = expiry.parse::<u64>() else {
        return Err((401, "bad upload expiry".into()));
    };
    let now = now_secs();
    if exp < now {
        return Err((401, "upload authorization expired; retry".into()));
    }
    if exp > now + 900 {
        return Err((401, "upload authorization expiry too far in the future".into()));
    }
    let wallet_body = if wallet_attempt {
        use crate::wallet_upload::{canonical_hex, message, verify};
        if !canonical_hex(&cfg.wallet_audience, 32, true) {
            return Err((401, "direct wallet uploads are not configured".into()));
        }
        let hash = req.header("x-upload-sha256").unwrap_or("");
        let size = req.header("content-length").and_then(|s| s.parse::<u64>().ok());
        if !canonical_hex(hash, 32, false) || size.is_none() || req.method != "POST"
            || !matches!(req.path.as_str(), "/add-json" | "/add-image" | "/add-wasm" | "/add-file") {
            return Err((401, "bad wallet upload scope".into()));
        }
        let size = size.unwrap();
        let msg = message(&cfg.wallet_audience, &req.path, &address, hash, size, exp);
        if !verify(&address, &msg, req.header("x-upload-signature").unwrap_or("")) {
            return Err((403, "invalid wallet upload signature".into()));
        }
        Some((hash.to_string(), size))
    } else {
        if cfg.upload_key.is_empty() { return Err((401, "direct wallet signature required".into())); }
        None
    };
    Ok(Some(AuthHdrs { address, expiry: exp, token, wallet_body }))
}

/// The api-relay's exact mint: HMAC-SHA256(UPLOAD_KEY, "addr:sha256hex:exp").
fn verify_body(auth: &AuthHdrs, upload_key: &str, body_sha256_hex: &str, size: u64) -> Result<(), (u16, String)> {
    if let Some((hash, expected_size)) = &auth.wallet_body {
        return if hash == body_sha256_hex && *expected_size == size { Ok(()) }
            else { Err((403, "upload signature does not cover these bytes".into())) };
    }
    let msg = format!("{}:{}:{}", auth.address, body_sha256_hex, auth.expiry);
    let mut mac = Hmac::<Sha256>::new_from_slice(upload_key.as_bytes()).expect("hmac key");
    mac.update(msg.as_bytes());
    let Some(token) = hex_decode(&auth.token) else {
        return Err((403, "upload authorization does not cover these bytes".into()));
    };
    mac.verify_slice(&token)
        .map_err(|_| (403, "upload authorization does not cover these bytes".into()))
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let val = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    (0..s.len() / 2)
        .map(|i| Some(val(b[2 * i])? * 16 + val(b[2 * i + 1])?))
        .collect()
}

// ---- CORS -------------------------------------------------------------------

/// Echo the request Origin when it's on the allowlist (a response can carry
/// only ONE allow-origin value); Vary so caches keep them apart.
pub fn cors(resp: Response, req: &Request, cfg: &UploadCfg) -> Response {
    let origin = req.header("origin").unwrap_or("").trim_end_matches('/');
    let allow = cfg
        .allow_origins
        .iter()
        .find(|o| o.as_str() == origin)
        .or(cfg.allow_origins.first());
    let Some(allow) = allow else { return resp };
    resp.with("access-control-allow-origin", allow)
        .with("vary", "Origin")
        .with("access-control-allow-methods", "POST, OPTIONS")
        .with(
            "access-control-allow-headers",
            "content-type, x-upload-address, x-upload-expiry, x-upload-token, x-upload-signature, x-upload-sha256, x-upload-scheme, x-upload-nonce",
        )
}

/// The gateway's CORS: a configured browser origin (exactly; no fallback, no `*`, no credentials) may READ the public,
/// content-addressed bytes - the dashboard's exact-byte readback after an upload, and its read of committed startup
/// settings, both send `range` and `cache-control` and read `content-range`. Nothing here authorizes anything.
pub fn gateway_cors(req: &Request, cfg: &UploadCfg) -> Vec<(String, String)> {
    let origin = req.header("origin").unwrap_or("").trim_end_matches('/');
    if origin.is_empty() || !cfg.allow_origins.iter().any(|o| o.as_str() == origin) {
        return Vec::new();
    }
    vec![
        ("access-control-allow-origin".into(), origin.to_string()),
        ("vary".into(), "Origin".into()),
        ("access-control-expose-headers".into(), "content-range, content-length, etag".into()),
    ]
}

/// The preflight for a gateway read (OPTIONS /ipfs/...).
pub fn gateway_preflight(req: &Request, cfg: Option<&UploadCfg>) -> Response {
    let mut resp = Response::new(204, "No Content");
    let cors = cfg.map(|c| gateway_cors(req, c)).unwrap_or_default();
    if !cors.is_empty() {
        resp.headers.extend(cors);
        resp = resp.with("access-control-allow-methods", "GET, HEAD, OPTIONS").with("access-control-allow-headers", "range, cache-control");
    }
    resp
}

// ---- pinning ----------------------------------------------------------------

/// Compute leaf digests for buffered bytes, kubo-chunked.
fn leaves_of(data: &[u8]) -> Vec<[u8; 32]> {
    if data.is_empty() {
        return vec![Sha256::digest(b"").into()];
    }
    data.chunks(CHUNK as usize)
        .map(|c| Sha256::digest(c).into())
        .collect()
}

/// PUT buffered bytes at pins/<cid> (skipping the write when the object
/// already exists with the right size) and queue the snapshot merge.
/// Returns the CID.
fn pin_buffered(app: &App, data: &[u8]) -> Result<Cid, String> {
    let s3ctx = app.s3.as_ref().expect("configured");
    let leaves = leaves_of(data);
    let (cid, _, _) = ipfs::build_file_dag(&leaves, data.len() as u64);
    let key = format!("{}{}{}", app.cfg.prefix, PIN_PREFIX, cid);
    let existing = s3::head_object(&s3ctx.ep, &s3ctx.bucket, &key, s3ctx.creds.as_ref())?;
    let etag = match existing {
        Some((size, etag)) if size == data.len() as u64 => etag,
        _ => s3::put_object_etag(&s3ctx.ep, &s3ctx.bucket, &key, s3ctx.creds.as_ref(), data)?,
    };
    app.upload_shared.borrow_mut().commits.push(PendingCommit {
        key,
        size: data.len() as u64,
        etag,
        leaves,
    });
    Ok(cid)
}

// ---- the buffered routes: /add-json, /add-image -----------------------------

fn refuse(srv: &mut Server, conn: usize, req: &Request, cfg: &UploadCfg, code: u16, msg: &str) {
    let reason = match code {
        401 => "Unauthorized",
        403 => "Forbidden",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Bad Gateway",
    };
    let resp = json(code, reason, format!("{{\"error\":\"{}\"}}", json_escape(msg)));
    srv.respond(conn, cors(resp, req, cfg));
}

/// Common front half of the buffered routes: configured? body present and
/// under cap? headers well-formed? HMAC covers these exact bytes? bytes
/// reserved? Returns the verified auth (None = auth disabled).
fn gate_buffered<'a>(
    app: &App,
    req: &'a Request,
    cap: usize,
) -> Result<Option<AuthHdrs>, (u16, String)> {
    let cfg = app.cfg.upload.as_ref().expect("gated by caller");
    if req.body.is_empty() {
        return Err((411, "Content-Length required".into()));
    }
    if req.body.len() > cap {
        return Err((413, format!("too large (max {cap} bytes)")));
    }
    let auth = precheck_with(cfg, req, Some(&app.upload_shared))?;
    if let Some(a) = &auth {
        let hash = hex_of(&Sha256::digest(&req.body));
        verify_body(a, &cfg.upload_key, &hash, req.body.len() as u64)?;
        reserve_bytes(
            &mut app.upload_shared.borrow_mut(),
            cfg.per_addr_daily,
            cfg.global_daily,
            &a.address,
            req.body.len() as u64,
        )
        .map_err(|m| (429u16, m))?;
    }
    Ok(auth)
}

fn hex_of(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn add_json(app: &mut App, srv: &mut Server, conn: usize, req: &Request) {
    let Some(cfg) = app.cfg.upload.as_ref().filter(|c| c.enabled()) else {
        return srv.respond(conn, json(503, "Service Unavailable", "{\"error\":\"uploads not configured\"}".into()));
    };
    if app.s3.is_none() {
        return refuse(srv, conn, req, cfg, 503, "unconfigured");
    }
    // The bucket stays the cheap pre-filter in front of the HMAC work.
    if !json_rate_ok(&mut app.upload_shared.borrow_mut(), cfg, req) {
        return refuse(srv, conn, req, cfg, 429, "too many config pins from your network; retry shortly");
    }
    let cap = cfg.max_json;
    match gate_buffered(app, req, cap) {
        Err((code, msg)) => {
            let cfg = app.cfg.upload.as_ref().unwrap();
            refuse(srv, conn, req, cfg, code, &msg)
        }
        Ok(_) => {
            // Must be a JSON OBJECT. The enclave re-fetches + hash-verifies
            // the CID, so this is UX/availability, not trust - but validate
            // the shape so a bad pin fails here.
            let cfg = app.cfg.upload.as_ref().unwrap();
            let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&req.body);
            match parsed {
                Err(e) => refuse(srv, conn, req, cfg, 415, &format!("not valid UTF-8 JSON: {e}")),
                Ok(v) if !v.is_object() => {
                    refuse(srv, conn, req, cfg, 415, "config must be a JSON object")
                }
                Ok(_) => match pin_buffered(app, &req.body) {
                    Err(e) => {
                        let cfg = app.cfg.upload.as_ref().unwrap();
                        refuse(srv, conn, req, cfg, 502, &format!("pin failed: {e}"))
                    }
                    Ok(cid) => {
                        let cfg = app.cfg.upload.as_ref().unwrap();
                        let resp = json(200, "OK", format!("{{\"cid\":\"{cid}\"}}"));
                        srv.respond(conn, cors(resp, req, cfg));
                    }
                },
            }
        }
    }
}

pub fn add_image(app: &mut App, srv: &mut Server, conn: usize, req: &Request) {
    let Some(cfg) = app.cfg.upload.as_ref().filter(|c| c.enabled()) else {
        return srv.respond(conn, json(503, "Service Unavailable", "{\"error\":\"uploads not configured\"}".into()));
    };
    if app.s3.is_none() {
        return refuse(srv, conn, req, cfg, 503, "unconfigured");
    }
    let cap = cfg.max_image;
    match gate_buffered(app, req, cap) {
        Err((code, msg)) => {
            let cfg = app.cfg.upload.as_ref().unwrap();
            refuse(srv, conn, req, cfg, code, &msg)
        }
        Ok(_) => {
            let (kind, err) = crate::imgcheck::image_error(&req.body);
            let cfg = app.cfg.upload.as_ref().unwrap();
            if let Some(e) = err {
                return refuse(srv, conn, req, cfg, 415, &e);
            }
            match pin_buffered(app, &req.body) {
                Err(e) => {
                    let cfg = app.cfg.upload.as_ref().unwrap();
                    refuse(srv, conn, req, cfg, 502, &format!("pin failed: {e}"))
                }
                Ok(cid) => {
                    let cfg = app.cfg.upload.as_ref().unwrap();
                    let svg = kind == Some("svg");
                    let resp = json(200, "OK", format!("{{\"cid\":\"{cid}\",\"svg\":{svg}}}"));
                    srv.respond(conn, cors(resp, req, cfg));
                }
            }
        }
    }
}

// ---- the streaming routes: /add-wasm and /add-file -----------------------------------------

/// Route entry: refuse everything refusable from the headers alone, then
/// register the sink that consumes the body.
pub fn add_wasm(app: &mut App, srv: &mut Server, conn: usize, req: &Request) {
    add_stream(app, srv, conn, req, true);
}

/// Runtime/kernel bytes are stored by CID, not accepted as an executable app.
/// Compute admission continues to validate the measured runtime independently.
pub fn add_file(app: &mut App, srv: &mut Server, conn: usize, req: &Request) {
    add_stream(app, srv, conn, req, false);
}

fn add_stream(app: &mut App, srv: &mut Server, conn: usize, req: &Request, wasm: bool) {
    let Some(cfg) = app.cfg.upload.as_ref().filter(|c| c.enabled()) else {
        return srv.respond(conn, json(503, "Service Unavailable", "{\"error\":\"uploads not configured\"}".into()));
    };
    let Some(s3ctx) = app.s3.clone() else {
        return refuse(srv, conn, req, cfg, 503, "unconfigured");
    };
    let Some(declared) = req.stream_len.filter(|&n| n > 0) else {
        return refuse(srv, conn, req, cfg, 411, "Content-Length required");
    };
    let max_bytes = if wasm { cfg.max_wasm } else { cfg.max_file };
    if !wasm && max_bytes == 0 {
        return refuse(srv, conn, req, cfg, 503, "file uploads are disabled by this operator");
    }
    if declared > max_bytes {
        return refuse(srv, conn, req, cfg, 413, &format!("too large (max {max_bytes} bytes)"));
    }
    let auth = match precheck_with(cfg, req, Some(&app.upload_shared)) {
        Err((code, msg)) => return refuse(srv, conn, req, cfg, code, &msg),
        Ok(a) => a,
    };
    {
        let mut sh = app.upload_shared.borrow_mut();
        if sh.slots >= UPLOAD_SLOTS {
            return refuse(srv, conn, req, cfg, 429, "too many concurrent uploads; retry shortly");
        }
        // Reserve the declared size against the FLEET daily budget up front,
        // before a single body byte (and so before any S3 multipart write on
        // the >8 MiB path). The token can only be checked once the whole body
        // has streamed, so without this a well-formed-but-forged token drives
        // unbounded pre-auth S3 churn - the daily ceiling never engaged
        // because it was only reserved on success. Global-only (never keyed
        // on the unverified address), and refunded on every failure/abort, so
        // it bounds cost without letting a forged address grief a wallet or
        // permanently burn the fleet budget.
        if let Err(msg) = reserve_global(&mut sh, cfg.global_daily, declared) {
            return refuse(srv, conn, req, cfg, 429, &msg);
        }
        sh.slots += 1;
        sh.seq += 1;
    }
    let seq = app.upload_shared.borrow().seq;
    let staging = format!(
        "{}{}{}-{}",
        app.cfg.prefix,
        STAGING_PREFIX,
        now_secs(),
        seq
    );
    // CORS decisions are per-request; the sink carries the computed headers
    // since its responses go out long after this function returns.
    let cors_hdrs: Vec<(String, String)> = {
        let probe = cors(Response::new(200, "OK"), req, cfg);
        probe.headers
    };
    let sink = ArtifactSink {
        s3: s3ctx,
        shared: app.upload_shared.clone(),
        upload_key: cfg.upload_key.clone(),
        auth,
        cors: cors_hdrs,
        pin_prefix: format!("{}{}", app.cfg.prefix, PIN_PREFIX),
        staging,
        per_addr_daily: cfg.per_addr_daily,
        global_held: declared, // the provisional fleet-budget reservation
        small: declared as usize <= PART_SIZE,
        buf: Vec::new(),
        upload_id: None,
        parts: Vec::new(),
        part_no: 0,
        fed: 0,
        body_hash: Sha256::new(),
        leaf_hash: Sha256::new(),
        leaf_fill: 0,
        leaves: Vec::new(),
        scan: if wasm { Some(WasmScan::new()) } else { None },
    };
    srv.begin_body(conn, Box::new(sink));
}

struct ArtifactSink {
    s3: Rc<S3Ctx>,
    shared: Rc<RefCell<Shared>>,
    upload_key: String,
    auth: Option<AuthHdrs>,
    cors: Vec<(String, String)>,
    pin_prefix: String,
    staging: String,
    per_addr_daily: u64,
    global_held: u64, // fleet-budget bytes reserved provisionally (0 once settled)
    small: bool,    // whole body fits one part: buffer, skip multipart
    buf: Vec<u8>,   // small: whole body; big: the part being filled
    upload_id: Option<String>,
    parts: Vec<(u32, String)>,
    part_no: u32,
    fed: u64,
    body_hash: Sha256,
    leaf_hash: Sha256,
    leaf_fill: usize,
    leaves: Vec<[u8; 32]>,
    scan: Option<WasmScan>,
}

impl ArtifactSink {
    fn resp(&self, code: u16, reason: &'static str, body: String) -> Response {
        let mut r = json(code, reason, body);
        for (k, v) in &self.cors {
            r = r.with(k, v);
        }
        r
    }

    fn fail(&mut self, code: u16, reason: &'static str, msg: &str) -> Response {
        self.cleanup_upstream();
        self.refund_global(); // a failed upload does not consume fleet budget
        self.resp(code, reason, format!("{{\"error\":\"{}\"}}", json_escape(msg)))
    }

    /// Release any still-provisional fleet-budget reservation. Idempotent
    /// (zeroes the held amount), so fail()/abort()/Drop can all call it.
    fn refund_global(&mut self) {
        let n = std::mem::take(&mut self.global_held);
        if n > 0 {
            release_global(&mut self.shared.borrow_mut(), n);
        }
    }

    fn cleanup_upstream(&mut self) {
        if let Some(id) = self.upload_id.take() {
            if let Err(e) =
                s3::abort_multipart(&self.s3.ep, &self.s3.bucket, &self.staging, self.s3.creds.as_ref(), &id)
            {
                eprintln!("[s3-ipfs-adapter] abort multipart {}: {e}", self.staging);
            }
        }
    }

    /// Flush self.buf as the next part (only called on the big path).
    fn flush_part(&mut self) -> Result<(), String> {
        if self.upload_id.is_none() {
            let id = s3::create_multipart(
                &self.s3.ep,
                &self.s3.bucket,
                &self.staging,
                self.s3.creds.as_ref(),
            )?;
            self.upload_id = Some(id);
        }
        self.part_no += 1;
        let etag = s3::upload_part(
            &self.s3.ep,
            &self.s3.bucket,
            &self.staging,
            self.s3.creds.as_ref(),
            self.upload_id.as_ref().unwrap(),
            self.part_no,
            &self.buf,
        )?;
        self.parts.push((self.part_no, etag));
        self.buf.clear();
        Ok(())
    }
}

impl Sink for ArtifactSink {
    fn feed(&mut self, mut data: &[u8]) -> Result<(), Response> {
        self.fed += data.len() as u64;
        self.body_hash.update(data);
        if let Some(scan) = self.scan.as_mut() {
            if let Err(msg) = scan.feed(data) {
                // The WASM route retains its preamble validation at byte 8.
                return Err(self.fail(415, "Unsupported Media Type", &msg));
            }
        }
        // Leaf hashing on kubo's chunk grid.
        let mut rest = data;
        while !rest.is_empty() {
            let take = (CHUNK as usize - self.leaf_fill).min(rest.len());
            self.leaf_hash.update(&rest[..take]);
            self.leaf_fill += take;
            if self.leaf_fill == CHUNK as usize {
                let h = std::mem::take(&mut self.leaf_hash);
                self.leaves.push(h.finalize().into());
                self.leaf_fill = 0;
            }
            rest = &rest[take..];
        }
        // Part accumulation.
        while !data.is_empty() {
            let take = (PART_SIZE - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if !self.small && self.buf.len() == PART_SIZE {
                if let Err(e) = self.flush_part() {
                    return Err(self.fail(502, "Bad Gateway", &format!("pin failed: {e}")));
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Response {
        if self.leaf_fill > 0 || self.leaves.is_empty() {
            let h = std::mem::take(&mut self.leaf_hash);
            self.leaves.push(h.finalize().into());
            self.leaf_fill = 0;
        }
        // The HMAC binds the minted token to these exact bytes.
        let hash = hex_of(&std::mem::take(&mut self.body_hash).finalize());
        if let Some(a) = self.auth.take() {
            if let Err((code, msg)) = verify_body(&a, &self.upload_key, &hash, self.fed) {
                let reason = if code == 401 { "Unauthorized" } else { "Forbidden" };
                return self.fail(code, reason, &msg);
            }
            // The fleet-budget bytes are already held (reserved at header
            // time on `declared`, which equals `fed` for a completed body):
            // settle that hold as consumed and add only the per-WALLET half,
            // now that the address is verified. A wallet over budget releases
            // the fleet hold too, so a refused upload consumes neither.
            // Bind in its own statement so the RefMut temporary is dropped
            // before self.fail() may take &mut self.
            let reserved =
                reserve_addr(&mut self.shared.borrow_mut(), self.per_addr_daily, &a.address, self.fed);
            if let Err(msg) = reserved {
                return self.fail(429, "Too Many Requests", &msg);
            }
            self.global_held = 0; // consumed: the successful upload keeps its fleet bytes
        }
        if let Some(msg) = self.scan.as_ref().and_then(WasmScan::accept_error) {
            return self.fail(415, "Unsupported Media Type", &msg);
        }
        let (cid, _, _) = ipfs::build_file_dag(&self.leaves, self.fed);
        let final_key = format!("{}{}", self.pin_prefix, cid);
        let existing = match s3::head_object(&self.s3.ep, &self.s3.bucket, &final_key, self.s3.creds.as_ref()) {
            Ok(e) => e,
            Err(e) => return self.fail(502, "Bad Gateway", &format!("pin failed: {e}")),
        };
        let etag = if let Some((_, etag)) = existing.filter(|(s, _)| *s == self.fed) {
            self.cleanup_upstream(); // duplicate: keep the existing object
            etag
        } else if self.small {
            match s3::put_object_etag(&self.s3.ep, &self.s3.bucket, &final_key, self.s3.creds.as_ref(), &self.buf) {
                Ok(etag) => etag,
                Err(e) => return self.fail(502, "Bad Gateway", &format!("pin failed: {e}")),
            }
        } else {
            // Tail part (any size), complete, rename to the CID, drop staging.
            if !self.buf.is_empty() || self.parts.is_empty() {
                if let Err(e) = self.flush_part() {
                    return self.fail(502, "Bad Gateway", &format!("pin failed: {e}"));
                }
            }
            let id = self.upload_id.take().expect("multipart started");
            if let Err(e) = s3::complete_multipart(&self.s3.ep, &self.s3.bucket, &self.staging, self.s3.creds.as_ref(), &id, &self.parts) {
                let _ = s3::abort_multipart(&self.s3.ep, &self.s3.bucket, &self.staging, self.s3.creds.as_ref(), &id);
                return self.fail(502, "Bad Gateway", &format!("pin failed: {e}"));
            }
            let copied = s3::copy_object(&self.s3.ep, &self.s3.bucket, &final_key, &self.staging, self.s3.creds.as_ref());
            let _ = s3::delete_object(&self.s3.ep, &self.s3.bucket, &self.staging, self.s3.creds.as_ref());
            match copied {
                Ok(etag) => etag,
                Err(e) => return self.fail(502, "Bad Gateway", &format!("pin failed: {e}")),
            }
        };
        self.shared.borrow_mut().commits.push(PendingCommit {
            key: final_key,
            size: self.fed,
            etag,
            leaves: std::mem::take(&mut self.leaves),
        });
        let Some(scan) = self.scan.as_ref() else {
            eprintln!("[s3-ipfs-adapter] pinned file {cid} ({} bytes)", self.fed);
            return self.resp(200, "OK", format!("{{\"cid\":\"{cid}\",\"bytes\":{}}}", self.fed));
        };
        let v = scan.verdict();
        eprintln!(
            "[s3-ipfs-adapter] pinned wasm {cid} ({} bytes, wasi {})",
            self.fed,
            v.wasi.unwrap_or("?")
        );
        self.resp(
            200,
            "OK",
            format!(
                "{{\"cid\":\"{cid}\",\"wasi\":{},\"world\":{},\"threads\":{},\"set\":{},\"mem64\":{}}}",
                v.wasi.map(|w| format!("\"{w}\"")).unwrap_or_else(|| "null".into()),
                v.world.map(|w| format!("\"{}\"", json_escape(&w))).unwrap_or_else(|| "null".into()),
                v.threads,
                v.set,
                v.mem64,
            ),
        )
    }

    fn abort(&mut self) {
        self.cleanup_upstream();
        self.refund_global(); // a connection that died mid-body consumed nothing
    }
}

impl Drop for ArtifactSink {
    fn drop(&mut self) {
        // Backstop: any reservation not settled by finish()/fail()/abort() is
        // refunded here (e.g. the sink dropped on a reap the loop did not route
        // through abort). borrow the shared state once for both.
        let held = std::mem::take(&mut self.global_held);
        let mut sh = self.shared.borrow_mut();
        sh.slots = sh.slots.saturating_sub(1);
        if held > 0 {
            release_global(&mut sh, held);
        }
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use k256::ecdsa::SigningKey;
    use sha3::Keccak256;
    fn cfg() -> UploadCfg {
        UploadCfg { upload_key: String::new(), wallet_audience: format!("0x{}", "ab".repeat(32)), allow_unsigned: false, allow_origins: vec![], max_wasm: 1024, max_file: 1024, max_image: 1024, max_json: 1024, per_addr_daily: 1024, global_daily: 4096, json_per_ip_hourly: 60.0, contract_wallets: None }
    }
    fn request(c: &UploadCfg, expiry: u64) -> Request { request_for(c, expiry, "/add-json") }
    fn request_for(c: &UploadCfg, expiry: u64, path: &str) -> Request {
        let v: serde_json::Value = serde_json::from_str(include_str!("wallet-upload-vector.json")).unwrap();
        let address = v["address"].as_str().unwrap();
        let hash = hex_of(&Sha256::digest(b"{}"));
        let m = crate::wallet_upload::message(&c.wallet_audience,path,address,&hash,2,expiry);
        let mut h = Keccak256::new(); h.update(format!("\x19Ethereum Signed Message:\n{}",m.len())); h.update(m.as_bytes());
        let key = SigningKey::from_bytes(&[0x42u8;32].into()).unwrap();
        let (sig, rec) = key.sign_prehash_recoverable(&h.finalize()).unwrap();
        let signature = format!("0x{}{:02x}",hex_of(&sig.to_bytes()),rec.to_byte()+27);
        Request { method:"POST".into(),path:path.into(),query:String::new(),body:b"{}".to_vec(),stream_len:None,headers:vec![("content-length".into(),"2".into()),("x-upload-address".into(),address.into()),("x-upload-expiry".into(),expiry.to_string()),("x-upload-sha256".into(),hash),("x-upload-signature".into(),signature)] }
    }
    #[test]
    fn gateway_reads_are_open_only_to_configured_origins() {
        let mut c = cfg();
        c.allow_origins = vec!["https://dash.example".into(), "http://localhost:5173".into()];
        let get = |origin: Option<&str>, method: &str| Request { method: method.into(), path: "/ipfs/bafkreia".into(), query: String::new(), body: Vec::new(), stream_len: None,
            headers: origin.map(|o| vec![("origin".to_string(), o.to_string())]).unwrap_or_default() };
        let h = gateway_cors(&get(Some("https://dash.example/"), "GET"), &c);
        let find = |h: &Vec<(String, String)>, k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(find(&h, "access-control-allow-origin").as_deref(), Some("https://dash.example"));
        assert!(find(&h, "access-control-expose-headers").unwrap().contains("content-range"));
        assert!(h.iter().all(|(k, _)| k != "access-control-allow-credentials"));
        // no fallback to the first origin (unlike the upload preflight), and nothing without an Origin
        assert!(gateway_cors(&get(Some("https://evil.example"), "GET"), &c).is_empty());
        assert!(gateway_cors(&get(None, "GET"), &c).is_empty());
        let p = gateway_preflight(&get(Some("http://localhost:5173"), "OPTIONS"), Some(&c));
        assert_eq!(p.status, 204);
        assert_eq!(find(&p.headers, "access-control-allow-headers").as_deref(), Some("range, cache-control"));
        assert_eq!(find(&p.headers, "access-control-allow-methods").as_deref(), Some("GET, HEAD, OPTIONS"));
        assert!(gateway_preflight(&get(Some("https://evil.example"), "OPTIONS"), Some(&c)).headers.iter().all(|(k, _)| !k.starts_with("access-control")));
        assert!(gateway_preflight(&get(Some("https://dash.example"), "OPTIONS"), None).headers.iter().all(|(k, _)| !k.starts_with("access-control")));
    }
    #[test]
    fn direct_upload_needs_no_relay_secret_and_binds_final_bytes() {
        let c=cfg(); assert!(c.enabled());
        let req=request(&c,now_secs()+300); let a=precheck(&c,&req).unwrap().unwrap();
        assert!(verify_body(&a,"",&hex_of(&Sha256::digest(b"{}")),2).is_ok());
        assert!(verify_body(&a,"",&hex_of(&Sha256::digest(b"[]")),2).is_err());
        assert!(verify_body(&a,"",&hex_of(&Sha256::digest(b"{}")),3).is_err());
        let mut unsigned=req; unsigned.headers.clear(); assert!(precheck(&c,&unsigned).is_err());
    }
    #[test]
    fn invalid_wallet_auth_cannot_fall_through_to_legacy_or_dev() {
        let mut c=cfg(); c.allow_unsigned=true; c.upload_key="legacy".into();
        let mut req=request(&c,now_secs()+300);
        req.headers.retain(|(k,_)|k!="x-upload-signature");
        req.headers.push(("x-upload-token".into(),"0".repeat(64)));
        assert!(precheck(&c,&req).is_err());
        c.upload_key.clear(); assert!(precheck(&c,&req).is_err());
        req.headers.clear(); assert!(precheck(&c,&req).unwrap().is_none());
    }
    #[test]
    fn rejects_expiry_scope_and_header_ambiguity() {
        let c=cfg();
        assert!(precheck(&c,&request(&c,now_secs()-1)).is_err());
        assert!(precheck(&c,&request(&c,now_secs()+1000)).is_err());
        let mut req=request(&c,now_secs()+300); req.path="/add-wasm".into(); assert!(precheck(&c,&req).is_err());
        let mut req=request(&c,now_secs()+300);req.headers.push(("x-upload-address".into(),"0x00".into()));assert!(precheck(&c,&req).is_err());
        let req=request(&c,now_secs()+300);let mut other=cfg();other.wallet_audience=format!("0x{}","cd".repeat(32));assert!(precheck(&other,&req).is_err());
    }
    #[test]
    fn generic_file_requires_its_own_wallet_scope_even_in_legacy_or_dev_mode() {
        let mut c=cfg();
        let req=request_for(&c,now_secs()+300,"/add-file");
        let auth=precheck(&c,&req).unwrap().unwrap();
        assert!(verify_body(&auth,"",&hex_of(&Sha256::digest(b"{}")),2).is_ok());
        let mut wrong=request(&c,now_secs()+300);wrong.path="/add-file".into();
        assert!(precheck(&c,&wrong).is_err());
        let mut unsigned=req;unsigned.headers.clear();c.allow_unsigned=true;
        assert!(precheck(&c,&unsigned).is_err());
        c.upload_key="legacy".into();unsigned.headers.push(("x-upload-token".into(),"00".repeat(32)));
        assert!(precheck(&c,&unsigned).is_err());
    }

    #[test]
    fn legacy_hmac_stays_compatible() {
        let mut c=cfg();c.upload_key="legacy".into();let mut req=request(&c,now_secs()+300);
        let addr=req.header("x-upload-address").unwrap();let expiry=req.header("x-upload-expiry").unwrap();let hash=hex_of(&Sha256::digest(b"{}"));
        let mut mac=Hmac::<Sha256>::new_from_slice(c.upload_key.as_bytes()).unwrap();mac.update(format!("{addr}:{hash}:{expiry}").as_bytes());let token=hex_of(&mac.finalize().into_bytes());
        req.headers.retain(|(k,_)|k!="x-upload-signature"&&k!="x-upload-sha256");req.headers.push(("x-upload-token".into(),token));
        let a=precheck(&c,&req).unwrap().unwrap();assert!(verify_body(&a,&c.upload_key,&hash,2).is_ok());assert!(verify_body(&a,"wrong",&hash,2).is_err());
        req.path="/add-file".into();assert!(precheck(&c,&req).is_err());
    }
}
