//! Smart-account upload authorization (`x-upload-scheme: erc1271-v2`): an upload signed by a CONTRACT account - a Safe
//! whose owners are passkeys, for example - which has no private key and so cannot make the v1 EIP-191 signature
//! (wallet_upload.rs). Additive: v1 wallet signatures and the legacy relay HMAC token are unchanged.
//!
//! The account signs (through its own contract, ERC-1271) the EIP-191 digest of this exact text:
//!
//!   enclave-storage-upload:v2
//!   scheme:erc1271
//!   chainId:<the chain the operator configured>
//!   service:<this adapter's deployment id>
//!   method:POST
//!   path:</add-json | /add-image | /add-wasm | /add-file>
//!   account:<0x + 40 lowercase hex>
//!   sha256:<the body's sha256>
//!   bytes:<the body's size>
//!   nonce:<32 lowercase hex, single use>
//!   expiry:<unix seconds, at most 900 s ahead>
//!
//! The adapter asks the account itself - isValidSignature(digest, signature) - through the OWNER-PINNED chain readers
//! (`upload.contractWallets`, the suite's `contract_wallets` grammar), and accepts only what the shared, reviewed
//! check_1271 accepts: an identical snapshot (block number, hash, time and the answer AT THAT HASH) held by at least
//! `quorum` distinct readers, no competing snapshot reaching quorum, fresh by the clock read after the reads. Nobody
//! else vouches: no relay, no issuer, no service signer.
//!
//! What is refused, and when (all before a single body byte):
//!   - not configured (no `upload.contractWallets`): 401, the adapter never guesses a chain or a reader;
//!   - malformed headers, an expired or too-distant expiry, a legacy token alongside, a scope this route does not take;
//!   - an ERC-6492 signature (an account not deployed yet): 401 "deploy the account first". Exact counterfactual
//!     validation would rely on simulating a factory call the adapter has not reviewed; a deployed account answers
//!     for itself;
//!   - a nonce already used by this account within its lifetime: 409;
//!   - more than CHECKS_PER_MINUTE chain checks in a minute (the event loop is single-threaded): 429;
//!   - a signature the account's contract does not accept, or readers that do not agree: 403.
//! The declared sha256 and size are checked against the body when it is complete (upload::verify_body), as for v1.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use sha3::{Digest, Keccak256};

use crate::contract_wallet::{check_1271, ChainReaders, ContractCfg, ERC6492_MAGIC};
use crate::httpd::Request;

pub const SCHEME: &str = "erc1271-v2";
pub const MAX_SIGNATURE_BYTES: usize = 6144;
/// Chain checks the adapter will run per minute, across all clients (each is a few reader round trips on a
/// single-threaded loop). Beyond it: 429, nothing read.
pub const CHECKS_PER_MINUTE: u32 = 30;
const MAX_AHEAD_SECS: u64 = 900;
const PATHS: [&str; 4] = ["/add-json", "/add-image", "/add-wasm", "/add-file"];

/// `upload.contractWallets`: the chain and the readers the OPERATOR chose for smart-account uploads.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountCfg {
    pub chain_id: u64,
    pub contract: ContractCfg,
}

impl AccountCfg {
    pub fn parse(v: &serde_json::Value) -> Result<AccountCfg, String> {
        if !v.is_object() {
            return Err("upload.contractWallets must be an object".into());
        }
        let chain_id = match v.get("chainId") {
            None | Some(serde_json::Value::Null) => 8453,
            Some(c) => c.as_u64().filter(|n| *n > 0).ok_or("upload.contractWallets.chainId must be a chain id")?,
        };
        let contract = ContractCfg::parse(v).map_err(|e| e.replace("wallet.contract_wallets", "upload.contractWallets"))?;
        Ok(AccountCfg { chain_id, contract })
    }
}

/// The exact text an account signs (see the module comment).
#[allow(clippy::too_many_arguments)]
pub fn message(chain_id: u64, service: &str, path: &str, account: &str, hash: &str, bytes: u64, nonce: &str, expiry: u64) -> String {
    format!("enclave-storage-upload:v2\nscheme:erc1271\nchainId:{chain_id}\nservice:{service}\nmethod:POST\npath:{path}\naccount:{account}\nsha256:{hash}\nbytes:{bytes}\nnonce:{nonce}\nexpiry:{expiry}")
}

/// The EIP-191 digest of `msg`: the hash isValidSignature is asked about.
pub fn digest(msg: &str) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(format!("\x19Ethereum Signed Message:\n{}", msg.len()));
    h.update(msg.as_bytes());
    h.finalize().into()
}

fn lower_hex(s: &str, digits: usize) -> bool {
    s.len() == digits && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// One smart-account upload request, parsed and checked from its headers alone.
#[derive(Debug, Clone)]
pub struct Claim {
    pub account: String,
    pub hash: String,
    pub size: u64,
    pub nonce: String,
    pub expiry: u64,
    pub signature: Vec<u8>,
    pub digest: [u8; 32],
}

/// Everything decidable without the network, at `now`. `audience` is this adapter's deployment id.
pub fn parse(cfg: Option<&AccountCfg>, audience: &str, req: &Request, now: u64) -> Result<Claim, (u16, String)> {
    let bad = |m: &str| Err((401u16, m.to_string()));
    if req.header("x-upload-scheme").map(str::trim) != Some(SCHEME) {
        return bad("unknown upload authorization scheme");
    }
    for name in ["x-upload-scheme", "x-upload-nonce"] {
        if req.headers.iter().filter(|(k, _)| k == name).count() > 1 {
            return bad("duplicate upload authorization header");
        }
    }
    if req.header("x-upload-token").is_some() {
        return bad("a smart-account upload carries no relay token");
    }
    let Some(cfg) = cfg else {
        return bad("smart-account uploads are not configured on this adapter (upload.contractWallets)");
    };
    if !(audience.len() == 66 && audience.starts_with("0x") && lower_hex(&audience[2..], 64)) {
        return bad("direct uploads are not configured");
    }
    let h = |n: &str| req.header(n).unwrap_or("").trim().to_string();
    let account = h("x-upload-address");
    if !(account.len() == 42 && account.starts_with("0x") && lower_hex(&account[2..], 40)) {
        return bad("bad upload account");
    }
    let nonce = h("x-upload-nonce");
    if !lower_hex(&nonce, 32) {
        return bad("bad upload nonce");
    }
    let Ok(expiry) = h("x-upload-expiry").parse::<u64>() else { return bad("bad upload expiry") };
    if expiry < now {
        return bad("upload authorization expired; retry");
    }
    if expiry > now + MAX_AHEAD_SECS {
        return bad("upload authorization expiry too far in the future");
    }
    let hash = h("x-upload-sha256");
    let size = req.header("content-length").and_then(|s| s.trim().parse::<u64>().ok());
    if !lower_hex(&hash, 64) || size.is_none() || req.method != "POST" || !PATHS.contains(&req.path.as_str()) {
        return bad("bad smart-account upload scope");
    }
    let sig_hex = h("x-upload-signature");
    let Some(raw) = sig_hex.strip_prefix("0x") else { return Err((403, "invalid smart-account upload signature".into())) };
    if raw.is_empty() || raw.len() % 2 != 0 || raw.len() > MAX_SIGNATURE_BYTES * 2 || !raw.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err((403, "invalid smart-account upload signature".into()));
    }
    let signature: Vec<u8> = (0..raw.len() / 2).map(|i| u8::from_str_radix(&raw[2 * i..2 * i + 2], 16).unwrap()).collect();
    if signature.ends_with(&ERC6492_MAGIC) {
        return bad("deploy the account first: this adapter accepts a smart-account upload only from a deployed account (an ERC-6492 signature is for one that is not deployed yet)");
    }
    let size = size.unwrap();
    let msg = message(cfg.chain_id, audience, &req.path, &account, &hash, size, &nonce, expiry);
    Ok(Claim { digest: digest(&msg), account, hash, size, nonce, expiry, signature })
}

/// Replay memory and the chain-check budget. One per process (the loop is single-threaded).
#[derive(Default)]
pub struct Guard {
    used: HashMap<String, u64>, // "account:nonce" -> expiry
    minute: u64,
    checks: u32,
}

impl Guard {
    fn prune(&mut self, now: u64) {
        self.used.retain(|_, exp| *exp >= now);
    }
    fn key(c: &Claim) -> String {
        format!("{}:{}", c.account, c.nonce)
    }
}

/// The decision for a parsed claim: replay, budget, then the account's own answer through the agreeing readers. Marks
/// the nonce used only after the account accepted, so a refused attempt cannot burn someone's nonce.
pub fn verify(cfg: &AccountCfg, c: &Claim, guard: &mut Guard, readers: &mut dyn ChainReaders) -> Result<(), (u16, String)> {
    let now = readers.now();
    {
        let g = &mut *guard;
        g.prune(now);
        if g.used.contains_key(&Guard::key(c)) {
            return Err((409, "this upload authorization was already used; sign a new one".into()));
        }
        if g.minute != now / 60 {
            g.minute = now / 60;
            g.checks = 0;
        }
        if g.checks >= CHECKS_PER_MINUTE {
            return Err((429, "too many smart-account upload checks right now; retry shortly".into()));
        }
        g.checks += 1;
    }
    check_1271(&cfg.contract, cfg.chain_id, &c.account, &c.digest, &c.signature, readers).map_err(|e| (403u16, e))?;
    // the readers took time: the authorization must still be alive when the decision is made
    let now = readers.now();
    if c.expiry < now {
        return Err((401, "upload authorization expired while it was being checked; retry".into()));
    }
    let g = guard;
    if g.used.contains_key(&Guard::key(c)) {
        return Err((409, "this upload authorization was already used; sign a new one".into()));
    }
    g.used.insert(Guard::key(c), c.expiry);
    Ok(())
}

// ---- the readers, over this app's own outbound network ------------------------------------------

/// JSON-RPC POSTs to the pinned readers through the app's egress (egress::dial), TLS by rustls with webpki roots
/// (https), plain TCP only for loopback test nodes (RpcUrl refuses plain http elsewhere).
pub struct NetReaders;

impl ChainReaders for NetReaders {
    fn post(&mut self, url: &str, body: &[u8], timeout_secs: u64, max_bytes: usize) -> Result<Vec<u8>, String> {
        rpc_post(url, body, Duration::from_secs(timeout_secs), max_bytes)
    }
    fn now(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

enum Wire {
    Plain(std::net::TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>),
}
impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self { Wire::Plain(s) => s.read(b), Wire::Tls(s) => s.read(b) }
    }
}
impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self { Wire::Plain(s) => s.write(b), Wire::Tls(s) => s.write(b) }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self { Wire::Plain(s) => s.flush(), Wire::Tls(s) => s.flush() }
    }
}

/// (https, host for TLS/Host header, dial host, port, path) of a reader URL that RpcUrl already accepted.
fn split_url(url: &str) -> Result<(bool, String, String, u16, String), String> {
    let r = crate::contract_wallet::RpcUrl::parse(url).map_err(|e| format!("reader url: {e}"))?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or("");
    let path = rest.find('/').map(|i| rest[i..].to_string()).unwrap_or_else(|| "/".into());
    let dial = r.host.trim_start_matches('[').trim_end_matches(']').to_string();
    Ok((r.https, r.host.clone(), dial, r.port, path))
}

fn rpc_post(url: &str, body: &[u8], timeout: Duration, max_bytes: usize) -> Result<Vec<u8>, String> {
    let (https, host, dial, port, path) = split_url(url)?;
    let deadline = std::time::Instant::now() + timeout + Duration::from_secs(2);
    let sock = crate::egress::dial(&dial, port, None)?;
    let _ = sock.set_read_timeout(Some(timeout));
    let _ = sock.set_write_timeout(Some(timeout));
    let mut wire = if https {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls versions: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(dial.clone()).map_err(|_| "bad TLS server name".to_string())?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| format!("tls setup: {e}"))?;
        Wire::Tls(Box::new(rustls::StreamOwned::new(conn, sock)))
    } else {
        Wire::Plain(sock)
    };
    let default_port = if https { 443 } else { 80 };
    let host_hdr = if port == default_port { host.clone() } else { format!("{host}:{port}") };
    let head = format!("POST {path} HTTP/1.1\r\nHost: {host_hdr}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    wire.write_all(head.as_bytes()).and_then(|_| wire.write_all(body)).and_then(|_| wire.flush()).map_err(|e| format!("reader write: {e}"))?;
    // read the whole answer (the peer closes), bounded by size and by time
    let cap = max_bytes + 16 * 1024;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if std::time::Instant::now() > deadline {
            return Err("reader timed out".into());
        }
        match wire.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > cap {
                    return Err("reader answered too much".into());
                }
            }
            // a TLS peer that closes without close_notify still delivered a complete HTTP answer
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
            Err(e) => return Err(format!("reader read: {e}")),
        }
    }
    parse_http(&raw, max_bytes)
}

/// The body of a complete HTTP/1.1 200 answer (content-length, chunked, or close-delimited).
fn parse_http(raw: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("reader sent no HTTP header")?;
    let head = std::str::from_utf8(&raw[..end]).map_err(|_| "reader sent a non-UTF-8 header")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(format!("reader answered {}", status.split_whitespace().nth(1).unwrap_or("?")));
    }
    let mut len = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" { len = v.parse::<usize>().ok(); }
            if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") { chunked = true; }
        }
    }
    let rest = &raw[end + 4..];
    let body = if chunked {
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let nl = rest[at..].windows(2).position(|w| w == b"\r\n").ok_or("reader sent a broken chunk")? + at;
            let size_txt = std::str::from_utf8(&rest[at..nl]).map_err(|_| "reader sent a broken chunk")?;
            let size = usize::from_str_radix(size_txt.split(';').next().unwrap_or("").trim(), 16).map_err(|_| "reader sent a broken chunk")?;
            at = nl + 2;
            if size == 0 { break; }
            if at + size > rest.len() || out.len() + size > max_bytes { return Err("reader sent a broken or oversized chunk".into()); }
            out.extend_from_slice(&rest[at..at + size]);
            at += size + 2;
        }
        out
    } else if let Some(n) = len {
        if n > rest.len() { return Err("reader answer ended early".into()); }
        rest[..n].to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > max_bytes {
        return Err("reader answered too much".into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUD: &str = "0xabababababababababababababababababababababababababababababababab";
    const ACCOUNT: &str = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    const NOW: u64 = 1_800_000_000;

    fn cfg() -> AccountCfg {
        AccountCfg::parse(&serde_json::json!({ "rpcs": ["http://127.0.0.1:8001", "http://127.0.0.1:8002", "http://127.0.0.1:8003"], "quorum": 2, "confirmations": 0 })).unwrap()
    }
    fn sha(b: &[u8]) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
    }
    fn req(path: &str, body: &[u8], nonce: &str, expiry: u64, sig: &str) -> Request {
        Request { method: "POST".into(), path: path.into(), query: String::new(), body: body.to_vec(), stream_len: None, headers: vec![
            ("content-length".into(), body.len().to_string()), ("x-upload-scheme".into(), SCHEME.into()), ("x-upload-address".into(), ACCOUNT.into()),
            ("x-upload-sha256".into(), sha(body)), ("x-upload-nonce".into(), nonce.into()), ("x-upload-expiry".into(), expiry.to_string()),
            ("x-upload-signature".into(), sig.into())] }
    }
    fn good() -> Request { req("/add-json", b"{}", &"0f".repeat(16), NOW + 300, &format!("0x{}", "51".repeat(97))) }

    /// Stand-in readers: each URL answers with its own head, block and verdict; `calls` records what was asked.
    struct Readers { answers: HashMap<String, (u64, String, u64, Option<bool>)>, chain: u64, now: u64, calls: Vec<String> }
    impl ChainReaders for Readers {
        fn post(&mut self, url: &str, body: &[u8], _: u64, _: usize) -> Result<Vec<u8>, String> {
            self.calls.push(String::from_utf8_lossy(body).to_string());
            let Some((head, hash, ts, valid)) = self.answers.get(url).cloned() else { return Err("down".into()) };
            let q: serde_json::Value = serde_json::from_slice(body).unwrap();
            let method = q[0]["method"].as_str().unwrap().to_string();
            let out = match method.as_str() {
                "eth_chainId" => serde_json::json!([{ "id": 1, "result": format!("0x{:x}", self.chain) }, { "id": 2, "result": { "number": format!("0x{head:x}") } }]),
                "eth_getBlockByNumber" => serde_json::json!([{ "id": 1, "result": { "number": q[0]["params"][0], "hash": hash, "timestamp": format!("0x{ts:x}") } }]),
                "eth_call" => match valid {
                    Some(true) => serde_json::json!([{ "id": 2, "result": format!("0x1626ba7e{}", "0".repeat(56)) }]),
                    Some(false) => serde_json::json!([{ "id": 2, "error": { "code": 3, "message": "execution reverted" } }]),
                    None => serde_json::json!([{ "id": 2, "result": "0xgarbage" }]),
                },
                _ => unreachable!(),
            };
            Ok(out.to_string().into_bytes())
        }
        fn now(&self) -> u64 { self.now }
    }
    fn readers(answers: &[(u64, &str, Option<bool>)]) -> Readers {
        let urls = ["http://127.0.0.1:8001", "http://127.0.0.1:8002", "http://127.0.0.1:8003"];
        Readers { answers: answers.iter().zip(urls).map(|((h, hash, v), u)| (u.to_string(), (*h, hash.to_string(), NOW - 2, *v))).collect(), chain: 8453, now: NOW, calls: vec![] }
    }
    const H: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "0x2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn message_binds_every_field_and_differs_from_v1() {
        let m = message(8453, AUD, "/add-json", ACCOUNT, &"cd".repeat(32), 2, &"0f".repeat(16), NOW);
        assert_eq!(m, format!("enclave-storage-upload:v2\nscheme:erc1271\nchainId:8453\nservice:{AUD}\nmethod:POST\npath:/add-json\naccount:{ACCOUNT}\nsha256:{}\nbytes:2\nnonce:{}\nexpiry:{NOW}", "cd".repeat(32), "0f".repeat(16)));
        let v1 = crate::wallet_upload::message(AUD, "/add-json", ACCOUNT, &"cd".repeat(32), 2, NOW);
        assert_ne!(digest(&m), digest(&v1));
        let base = digest(&m);
        for other in [message(1, AUD, "/add-json", ACCOUNT, &"cd".repeat(32), 2, &"0f".repeat(16), NOW),
                      message(8453, &format!("0x{}", "ef".repeat(32)), "/add-json", ACCOUNT, &"cd".repeat(32), 2, &"0f".repeat(16), NOW),
                      message(8453, AUD, "/add-wasm", ACCOUNT, &"cd".repeat(32), 2, &"0f".repeat(16), NOW),
                      message(8453, AUD, "/add-json", &format!("0x{}", "5b".repeat(20)), &"cd".repeat(32), 2, &"0f".repeat(16), NOW),
                      message(8453, AUD, "/add-json", ACCOUNT, &"00".repeat(32), 2, &"0f".repeat(16), NOW),
                      message(8453, AUD, "/add-json", ACCOUNT, &"cd".repeat(32), 3, &"0f".repeat(16), NOW),
                      message(8453, AUD, "/add-json", ACCOUNT, &"cd".repeat(32), 2, &"0e".repeat(16), NOW),
                      message(8453, AUD, "/add-json", ACCOUNT, &"cd".repeat(32), 2, &"0f".repeat(16), NOW + 1)] {
            assert_ne!(digest(&other), base, "{other}");
        }
    }

    #[test]
    fn headers_alone_refuse_everything_refusable() {
        let c = cfg();
        assert!(parse(Some(&c), AUD, &good(), NOW).is_ok());
        assert_eq!(parse(None, AUD, &good(), NOW).unwrap_err().0, 401, "not configured: never guessed");
        let with = |f: &dyn Fn(&mut Request)| { let mut r = good(); f(&mut r); parse(Some(&c), AUD, &r, NOW) };
        assert!(with(&|r| r.headers.retain(|(k, _)| k != "x-upload-scheme")).is_err(), "the scheme is explicit");
        assert!(with(&|r| r.headers.push(("x-upload-scheme".into(), SCHEME.into()))).is_err(), "duplicate");
        assert!(with(&|r| r.headers.push(("x-upload-token".into(), "00".repeat(32)))).is_err(), "no token alongside");
        assert!(with(&|r| r.method = "PUT".into()).is_err(), "method");
        assert!(with(&|r| r.path = "/add-other".into()).is_err(), "path");
        assert!(with(&|r| { r.headers.retain(|(k, _)| k != "x-upload-address"); r.headers.push(("x-upload-address".into(), ACCOUNT.to_uppercase().replace("0X", "0x"))) }).is_err(), "canonical account");
        assert!(with(&|r| { r.headers.retain(|(k, _)| k != "x-upload-nonce"); r.headers.push(("x-upload-nonce".into(), "0f".repeat(8))) }).is_err(), "nonce size");
        assert!(parse(Some(&c), AUD, &req("/add-json", b"{}", &"0f".repeat(16), NOW - 1, "0x51"), NOW).is_err(), "expired");
        assert!(parse(Some(&c), AUD, &req("/add-json", b"{}", &"0f".repeat(16), NOW + 901, "0x51"), NOW).is_err(), "too far ahead");
        assert_eq!(parse(Some(&c), AUD, &req("/add-json", b"{}", &"0f".repeat(16), NOW + 60, "0x"), NOW).unwrap_err().0, 403, "empty signature");
        assert_eq!(parse(Some(&c), AUD, &req("/add-json", b"{}", &"0f".repeat(16), NOW + 60, &format!("0x{}", "11".repeat(MAX_SIGNATURE_BYTES + 1))), NOW).unwrap_err().0, 403, "oversized");
        let wrapped = format!("0x{}{}", "11".repeat(96), ERC6492_MAGIC.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let e = parse(Some(&c), AUD, &req("/add-json", b"{}", &"0f".repeat(16), NOW + 60, &wrapped), NOW).unwrap_err();
        assert!(e.0 == 401 && e.1.contains("deploy the account first"), "{e:?}");
        assert!(parse(Some(&c), "0xshort", &good(), NOW).is_err(), "no audience, no direct uploads");
    }

    #[test]
    fn the_accounts_own_answer_by_agreeing_readers_and_single_use() {
        let c = cfg();
        let claim = parse(Some(&c), AUD, &good(), NOW).unwrap();
        let mut g = Guard::default();
        // two of three agree it is valid at the same block: accepted; the call asked is isValidSignature(digest, sig)
        let mut r = readers(&[(100, H, Some(true)), (100, H, Some(true)), (100, H, Some(false))]);
        verify(&c, &claim, &mut g, &mut r).unwrap();
        let call = r.calls.iter().find(|q| q.contains("eth_call")).unwrap();
        assert!(call.contains(&format!("0x1626ba7e{}", claim.digest.iter().map(|b| format!("{b:02x}")).collect::<String>())), "{call}");
        assert!(call.contains(ACCOUNT));
        // the same authorization again: replay refused, no chain read
        let n = r.calls.len();
        assert_eq!(verify(&c, &claim, &mut g, &mut r).unwrap_err().0, 409);
        assert_eq!(r.calls.len(), n, "a replay costs no reads");
        // a new nonce, but the account's contract says no at quorum: refused, and the nonce is not burnt
        let other = parse(Some(&c), AUD, &req("/add-json", b"{}", &"1e".repeat(16), NOW + 300, "0x51"), NOW).unwrap();
        let e = verify(&c, &other, &mut g, &mut readers(&[(100, H, Some(false)), (100, H, Some(false)), (100, H, Some(true))])).unwrap_err();
        assert!(e.0 == 403 && e.1.contains("does not accept"), "{e:?}");
        verify(&c, &other, &mut g, &mut readers(&[(100, H, Some(true)), (100, H, Some(true)), (100, H, None)])).unwrap();
    }

    #[test]
    fn inconsistent_readers_wrong_chain_and_stale_blocks_refuse() {
        let c = cfg();
        let fresh = |n: &str| parse(Some(&c), AUD, &req("/add-json", b"{}", &n.repeat(16), NOW + 300, "0x51"), NOW).unwrap();
        let mut g = Guard::default();
        // a split: two different blocks, no quorum for one answer
        let e = verify(&c, &fresh("01"), &mut g, &mut readers(&[(100, H, Some(true)), (100, H2, Some(true)), (100, H, None)])).unwrap_err();
        assert!(e.0 == 403 && e.1.contains("did not agree"), "{e:?}");
        // one reader lying "valid" is one vote short
        assert!(verify(&c, &fresh("02"), &mut g, &mut readers(&[(100, H, Some(true)), (100, H, Some(false)), (100, H, Some(false))])).is_err());
        // readers on another chain take no part
        let mut r = readers(&[(100, H, Some(true)), (100, H, Some(true)), (100, H, Some(true))]);
        r.chain = 1;
        assert!(verify(&c, &fresh("03"), &mut g, &mut r).unwrap_err().1.contains("too few chain readers"));
        // a stale agreed block
        let mut r = readers(&[(100, H, Some(true)), (100, H, Some(true)), (100, H, Some(true))]);
        r.now = NOW + 10_000;
        let late = parse(Some(&c), AUD, &req("/add-json", b"{}", &"04".repeat(16), NOW + 300, "0x51"), NOW).unwrap();
        assert!(verify(&c, &late, &mut g, &mut r).is_err());
    }

    #[test]
    fn the_check_budget_is_bounded() {
        let c = cfg();
        let mut g = Guard::default();
        for i in 0..CHECKS_PER_MINUTE {
            let cl = parse(Some(&c), AUD, &req("/add-json", b"{}", &format!("{i:032x}"), NOW + 300, "0x51"), NOW).unwrap();
            let _ = verify(&c, &cl, &mut g, &mut readers(&[(100, H, Some(false)), (100, H, Some(false)), (100, H, Some(false))]));
        }
        let cl = parse(Some(&c), AUD, &req("/add-json", b"{}", &"ff".repeat(16), NOW + 300, "0x51"), NOW).unwrap();
        let mut r = readers(&[(100, H, Some(true)), (100, H, Some(true)), (100, H, Some(true))]);
        assert_eq!(verify(&c, &cl, &mut g, &mut r).unwrap_err().0, 429);
        assert!(r.calls.is_empty(), "over budget: nothing read");
    }

    #[test]
    fn configuration_is_the_shared_grammar() {
        assert!(AccountCfg::parse(&serde_json::json!({ "rpcs": ["https://a.example"], "quorum": 2 })).is_err(), "at least two readers");
        assert!(AccountCfg::parse(&serde_json::json!({ "rpcs": ["https://a.example/x", "https://a.example/y"] })).unwrap_err().contains("upload.contractWallets"), "same host twice");
        assert!(AccountCfg::parse(&serde_json::json!({ "rpcs": ["http://rpc.example", "https://b.example"] })).is_err(), "plain http off loopback");
        assert!(AccountCfg::parse(&serde_json::json!({ "rpcs": ["https://a.example/?key=1", "https://b.example"] })).is_err(), "no query");
        let c = AccountCfg::parse(&serde_json::json!({ "chainId": 84532, "rpcs": ["https://a.example", "https://b.example"] })).unwrap();
        assert_eq!(c.chain_id, 84532); assert_eq!(c.contract.quorum, 2);
        assert!(AccountCfg::parse(&serde_json::json!({ "chainId": 0, "rpcs": ["https://a.example", "https://b.example"] })).is_err());
    }

    #[test]
    fn http_answers_are_parsed_strictly() {
        assert_eq!(parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}", 100).unwrap(), b"{}");
        assert_eq!(parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n1\r\n}\r\n0\r\n\r\n", 100).unwrap(), b"{}");
        assert!(parse_http(b"HTTP/1.1 500 Oops\r\n\r\n", 100).is_err());
        assert!(parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}", 100).is_err());
        assert!(parse_http(b"HTTP/1.1 200 OK\r\n\r\n0123456789", 5).is_err());
    }

    #[test]
    fn copied_verbatim_from_the_shared_wallet_module() {
        let file = include_str!("contract_wallet.rs");
        let body = file.split_once("#![allow(dead_code)]\n\n").unwrap().1.trim_end_matches('\n');
        use sha2::Digest as _;
        let h: String = sha2::Sha256::digest(body.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(h, "f25c4e669b7906140e954cf20c54e8f91a0624eeceb8a92a2a54d3c96693173f", "contract_wallet.rs drifted from the shared wallet.rs sections");
    }
}
