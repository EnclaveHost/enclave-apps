//! The pin routes: /add-wasm, /add-json, /add-image — the Rust port of the
//! nan box's validating upload gateway (scripts/ipfs-add-gateway.py), moved
//! inside the enclave so ipfs.enclave.host can be served entirely by this
//! app. Wire compatibility is the contract: same routes, same
//! x-upload-address / x-upload-expiry / x-upload-token wallet-HMAC auth
//! (minted by the api-relay, which is unchanged), same response shapes
//! ({"cid", ...}), same refusal codes (401/403/413/415/429), same CORS echo.
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
    pub allow_unsigned: bool, // dev/e2e only: no key, uploads open
    pub allow_origins: Vec<String>,
    pub max_wasm: u64,
    pub max_image: usize,
    pub max_json: usize,
    pub per_addr_daily: u64,
    pub global_daily: u64,
    pub json_per_ip_hourly: f64,
}

impl UploadCfg {
    pub fn enabled(&self) -> bool {
        !self.upload_key.is_empty() || self.allow_unsigned
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
        }))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reserve nbytes against the per-wallet + global daily caps.
fn reserve_bytes(
    sh: &mut Shared,
    per_addr_daily: u64,
    global_daily: u64,
    address: &str,
    n: u64,
) -> Result<(), String> {
    let day = now_secs() / 86400;
    if sh.day != day {
        sh.day = day;
        sh.global = 0;
        sh.addr.clear();
    }
    if sh.global + n > global_daily {
        return Err("fleet daily upload limit reached; retry tomorrow".into());
    }
    let used = sh.addr.get(address).copied().unwrap_or(0);
    if used + n > per_addr_daily {
        return Err("this wallet's daily upload limit reached; retry tomorrow".into());
    }
    sh.global += n;
    sh.addr.insert(address.to_string(), used + n);
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
}

/// Header-shape gate, decidable before any body byte: presence, address and
/// expiry formats, freshness. Returns Ok(None) when auth is disabled
/// (allow_unsigned dev mode). The HMAC itself binds to sha256(body), so it
/// is checked in `verify_hmac` once the body is complete.
fn precheck(cfg: &UploadCfg, req: &Request) -> Result<Option<AuthHdrs>, (u16, String)> {
    if cfg.upload_key.is_empty() {
        return Ok(None); // allow_unsigned checked by enabled()
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
    if address.is_empty() || expiry.is_empty() || token.is_empty() {
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
    Ok(Some(AuthHdrs { address, expiry: exp, token }))
}

/// The api-relay's exact mint: HMAC-SHA256(UPLOAD_KEY, "addr:sha256hex:exp").
fn verify_hmac(auth: &AuthHdrs, upload_key: &str, body_sha256_hex: &str) -> Result<(), (u16, String)> {
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
            "content-type, x-upload-address, x-upload-expiry, x-upload-token",
        )
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
    let auth = precheck(cfg, req)?;
    if let Some(a) = &auth {
        let hash = hex_of(&Sha256::digest(&req.body));
        verify_hmac(a, &cfg.upload_key, &hash)?;
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

// ---- the streaming route: /add-wasm -----------------------------------------

/// Route entry: refuse everything refusable from the headers alone, then
/// register the sink that consumes the body.
pub fn add_wasm(app: &mut App, srv: &mut Server, conn: usize, req: &Request) {
    let Some(cfg) = app.cfg.upload.as_ref().filter(|c| c.enabled()) else {
        return srv.respond(conn, json(503, "Service Unavailable", "{\"error\":\"uploads not configured\"}".into()));
    };
    let Some(s3ctx) = app.s3.clone() else {
        return refuse(srv, conn, req, cfg, 503, "unconfigured");
    };
    let Some(declared) = req.stream_len.filter(|&n| n > 0) else {
        return refuse(srv, conn, req, cfg, 411, "Content-Length required");
    };
    if declared > cfg.max_wasm {
        return refuse(srv, conn, req, cfg, 413, &format!("too large (max {} bytes)", cfg.max_wasm));
    }
    let auth = match precheck(cfg, req) {
        Err((code, msg)) => return refuse(srv, conn, req, cfg, code, &msg),
        Ok(a) => a,
    };
    {
        let mut sh = app.upload_shared.borrow_mut();
        if sh.slots >= UPLOAD_SLOTS {
            return refuse(srv, conn, req, cfg, 429, "too many concurrent uploads; retry shortly");
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
    let sink = WasmSink {
        s3: s3ctx,
        shared: app.upload_shared.clone(),
        upload_key: cfg.upload_key.clone(),
        auth,
        cors: cors_hdrs,
        pin_prefix: format!("{}{}", app.cfg.prefix, PIN_PREFIX),
        staging,
        per_addr_daily: cfg.per_addr_daily,
        global_daily: cfg.global_daily,
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
        scan: WasmScan::new(),
    };
    srv.begin_body(conn, Box::new(sink));
}

struct WasmSink {
    s3: Rc<S3Ctx>,
    shared: Rc<RefCell<Shared>>,
    upload_key: String,
    auth: Option<AuthHdrs>,
    cors: Vec<(String, String)>,
    pin_prefix: String,
    staging: String,
    per_addr_daily: u64,
    global_daily: u64,
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
    scan: WasmScan,
}

impl WasmSink {
    fn resp(&self, code: u16, reason: &'static str, body: String) -> Response {
        let mut r = json(code, reason, body);
        for (k, v) in &self.cors {
            r = r.with(k, v);
        }
        r
    }

    fn fail(&mut self, code: u16, reason: &'static str, msg: &str) -> Response {
        self.cleanup_upstream();
        self.resp(code, reason, format!("{{\"error\":\"{}\"}}", json_escape(msg)))
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

impl Sink for WasmSink {
    fn feed(&mut self, mut data: &[u8]) -> Result<(), Response> {
        self.fed += data.len() as u64;
        self.body_hash.update(data);
        if let Err(msg) = self.scan.feed(data) {
            // Tier-1 preamble refusal, decidable at byte 8: stop the upload
            // now instead of accepting gigabytes that can only be refused.
            return Err(self.fail(415, "Unsupported Media Type", &msg));
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
            if let Err((code, msg)) = verify_hmac(&a, &self.upload_key, &hash) {
                let reason = if code == 401 { "Unauthorized" } else { "Forbidden" };
                return self.fail(code, reason, &msg);
            }
            let reserved = reserve_bytes(
                &mut self.shared.borrow_mut(),
                self.per_addr_daily,
                self.global_daily,
                &a.address,
                self.fed,
            );
            if let Err(msg) = reserved {
                return self.fail(429, "Too Many Requests", &msg);
            }
        }
        if let Some(msg) = self.scan.accept_error() {
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
        let v = self.scan.verdict();
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
    }
}

impl Drop for WasmSink {
    fn drop(&mut self) {
        let mut sh = self.shared.borrow_mut();
        sh.slots = sh.slots.saturating_sub(1);
    }
}
