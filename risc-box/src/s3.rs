//! Minimal S3 client for risc-box: GET an object (the OS images) and PUT one
//! back (the guest-modified disk), over the platform's transparent egress.
//!
//! - `https://` endpoints use rustls with the pure-Rust RustCrypto provider —
//!   the only TLS stack that builds for wasm32-wasip2 — with webpki roots.
//!   `http://` endpoints use a plain TcpStream (for local mocks/minio).
//! - Requests are path-style (`/bucket/key`), which every S3-compatible
//!   store accepts and which keeps TLS SNI independent of bucket names.
//! - With credentials, requests are SigV4-signed (GET uses the empty-body
//!   hash, PUT hashes the real payload). Without credentials, requests go
//!   unsigned — public buckets and mock servers.
//!
//! No chrono, no aws-sdk: the date math and the signing chain are a page of
//! std + sha2/hmac.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

pub struct Endpoint {
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub region: String,
}

impl Endpoint {
    /// Parse "https://s3.eu-central-1.wasabisys.com" / "http://127.0.0.1:9000".
    pub fn parse(url: &str, region: &str) -> Result<Endpoint, String> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("endpoint must be http(s)://…, got {url}"));
        };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') {
            return Err("endpoint must be scheme://host[:port] with no path".into());
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), p.parse().map_err(|_| "bad port")?)
            }
            _ => (rest.to_string(), if https { 443 } else { 80 }),
        };
        Ok(Endpoint { https, host, port, region: region.to_string() })
    }
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// (YYYYMMDD, YYYYMMDDTHHMMSSZ) in UTC, from the system clock.
/// Civil-from-days per Howard Hinnant's algorithm.
fn amz_dates() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    let date = format!("{:04}{:02}{:02}", y, mo, d);
    let stamp = format!("{date}T{h:02}{m:02}{s:02}Z");
    (date, stamp)
}

/// Conservative URI-encode of an object key, segment slashes preserved
/// (SigV4 canonical URI wants each path segment encoded).
fn encode_key(key: &str) -> String {
    let mut out = String::new();
    for &b in key.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the SigV4 Authorization + x-amz-* headers for a request.
fn sign(
    method: &str,
    ep: &Endpoint,
    canonical_uri: &str,
    payload_hash: &str,
    creds: &Creds,
) -> Vec<(String, String)> {
    let (date, stamp) = amz_dates();
    let host_header = if (ep.https && ep.port == 443) || (!ep.https && ep.port == 80) {
        ep.host.clone()
    } else {
        format!("{}:{}", ep.host, ep.port)
    };
    let mut headers = vec![
        ("host".to_string(), host_header),
        ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
        ("x-amz-date".to_string(), stamp.clone()),
    ];
    if let Some(tok) = &creds.session_token {
        headers.push(("x-amz-security-token".to_string(), tok.clone()));
    }
    headers.sort();
    let signed_names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed_list = signed_names.join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_list}\n{payload_hash}"
    );
    let scope = format!("{date}/{}/s3/aws4_request", ep.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, ep.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_list}, Signature={signature}",
        creds.access_key_id
    );
    headers.retain(|(k, _)| k != "host"); // host goes out via the request line block below
    headers.push(("authorization".to_string(), auth));
    headers
}

/// Either side of the optional TLS wrap, unified for request().
enum Wire {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.read(buf),
            Wire::Tls(s) => s.read(buf),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(s) => s.write(buf),
            Wire::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(s) => s.flush(),
            Wire::Tls(s) => s.flush(),
        }
    }
}

fn connect(ep: &Endpoint) -> Result<Wire, String> {
    let sock = crate::egress::dial(ep.host.as_str(), ep.port, None)?;
    if !ep.https {
        return Ok(Wire::Plain(sock));
    }
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls versions: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(ep.host.clone())
        .map_err(|_| format!("bad TLS server name {}", ep.host))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| format!("tls setup: {e}"))?;
    Ok(Wire::Tls(Box::new(rustls::StreamOwned::new(conn, sock))))
}

/// What one request got. `cut` names why the body stopped short (a read error or EOF before Content-Length);
/// `body` then holds the bytes that did arrive, so a GET can ask for the rest.
struct Raw {
    status: u16,
    body: Vec<u8>,
    content_length: Option<usize>,
    /// the first byte this body starts at, from a 206's Content-Range
    range_start: Option<usize>,
    /// the object's ETag: a resume sends it as If-Range, so a changed object comes back whole (200), never spliced
    etag: Option<String>,
    cut: Option<String>,
}

/// One S3 request. Returns (status, body). `progress` is called with
/// (bytes_so_far, content_length_if_known) while the body streams in.
fn request(
    method: &str,
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    body: &[u8],
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(u16, Vec<u8>), String> {
    let raw = fetch_raw(method, ep, bucket, key, creds, body, 0, None, progress)?;
    match raw.cut {
        Some(why) => Err(why),
        None => Ok((raw.status, raw.body)),
    }
}

/// `request` that can start the body at `range_from` (`Range: bytes=N-`, unsigned: SigV4 does not need it) and
/// hands back a body cut short instead of discarding it.
fn fetch_raw(
    method: &str,
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    body: &[u8],
    range_from: usize,
    if_range: Option<&str>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Raw, String> {
    let canonical_uri = format!("/{}/{}", encode_key(bucket), encode_key(key));
    let payload_hash = match method {
        "PUT" => hex(&Sha256::digest(body)),
        _ => EMPTY_SHA256.to_string(),
    };
    let host_header = if (ep.https && ep.port == 443) || (!ep.https && ep.port == 80) {
        ep.host.clone()
    } else {
        format!("{}:{}", ep.host, ep.port)
    };
    let mut head = format!("{method} {canonical_uri} HTTP/1.1\r\nhost: {host_header}\r\n");
    match creds {
        Some(c) => {
            for (k, v) in sign(method, ep, &canonical_uri, &payload_hash, c) {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        None => {
            // unsigned (public bucket / mock): still send the content hash —
            // some stores want it on PUT
            if method == "PUT" {
                head.push_str(&format!("x-amz-content-sha256: {payload_hash}\r\n"));
            }
        }
    }
    if range_from > 0 {
        head.push_str(&format!("range: bytes={range_from}-\r\n"));
        if let Some(tag) = if_range {
            head.push_str(&format!("if-range: {tag}\r\n"));
        }
    }
    head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", body.len()));

    let mut wire = connect(ep)?;
    wire.write_all(head.as_bytes()).map_err(|e| format!("send: {e}"))?;
    // 64 KiB body chunks keep the TLS record path and wasi write sizes sane
    for chunk in body.chunks(64 * 1024) {
        wire.write_all(chunk).map_err(|e| format!("send body: {e}"))?;
    }
    wire.flush().ok();

    // Read the full response (headers + body).
    let mut rbuf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 64 * 1024];
    let mut head_end: Option<usize> = None;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut status: u16 = 0;
    let mut range_start: Option<usize> = None;
    let mut etag: Option<String> = None;
    let mut cut: Option<String> = None;
    loop {
        match wire.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                rbuf.extend_from_slice(&tmp[..n]);
                if head_end.is_none() {
                    if let Some(pos) = rbuf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = Some(pos + 4);
                        let head_text = String::from_utf8_lossy(&rbuf[..pos]).to_string();
                        let mut lines = head_text.split("\r\n");
                        status = lines
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|s| s.parse().ok())
                            .ok_or("bad status line")?;
                        for line in lines {
                            let Some((k, v)) = line.split_once(':') else { continue };
                            let k = k.trim().to_ascii_lowercase();
                            let v = v.trim();
                            if k == "content-length" {
                                content_length = v.parse().ok();
                            }
                            if k == "etag" && !v.is_empty() {
                                etag = Some(v.to_string());
                            }
                            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                                chunked = true;
                            }
                            // "bytes 1000-1999/5000": where a 206's body starts
                            if k == "content-range" {
                                range_start = v
                                    .strip_prefix("bytes ")
                                    .and_then(|r| r.split('-').next())
                                    .and_then(|n| n.trim().parse().ok());
                            }
                        }
                        if let Some(cl) = content_length {
                            rbuf.reserve_exact((pos + 4 + cl).saturating_sub(rbuf.len()));
                        }
                    }
                }
                // Progress is for the object, not for an error page: a 404's
                // few bytes of XML must not log as "100% of 127 bytes".
                if let (Some(he), Some(cl)) = (head_end, content_length) {
                    if status == 200 || status == 206 {
                        progress(rbuf.len() - he, cl);
                    }
                    if rbuf.len() >= he + cl {
                        break;
                    }
                } else if head_end.is_some() && (status == 200 || status == 206) {
                    progress(rbuf.len() - head_end.unwrap(), 0);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // TLS close-notify omission and plain EOF both end the body
            Err(e) if head_end.is_some() && content_length.is_none() && !chunked => {
                let _ = e;
                break;
            }
            // A known-length body cut mid-way is kept, not thrown away: get_resumable asks for the rest.
            Err(e) if head_end.is_some() && content_length.is_some() && !chunked => {
                cut = Some(format!("read: {e}"));
                break;
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let he = head_end.ok_or("response ended before headers completed")?;
    // The body IS the buffer past the headers: trim them off in place rather than copying the body out (for a
    // disk image that copy was a second gigabyte alive at once).
    let body = if chunked {
        dechunk(&rbuf[he..])?
    } else {
        rbuf.drain(..he);
        rbuf
    };
    if let Some(cl) = content_length {
        if body.len() < cl && cut.is_none() {
            cut = Some(format!("short body: {} of {cl} bytes", body.len()));
        }
    }
    Ok(Raw { status, body, content_length, range_start, etag, cut })
}

/// A download cut short and not finished, kept across `get_resumable` calls (one at a time: objects are fetched one
/// after another). The app's own retry used to call again from byte zero, so a privacy circuit that stayed down for
/// longer than three quick resumes cost the whole object again - on the NucBox, ~1 GB of paid TUNA bandwidth per drop
/// (2026-10-12: 182 MB, then 43 MB, then 60 MB of the same image). Now the next call for the same object picks up here.
struct Partial {
    bucket: String,
    key: String,
    etag: Option<String>,
    total: Option<usize>,
    have: Vec<u8>,
}
static PARTIAL: std::sync::Mutex<Option<Partial>> = std::sync::Mutex::new(None);

fn take_partial(bucket: &str, key: &str) -> Option<Partial> {
    let mut slot = PARTIAL.lock().ok()?;
    match slot.as_ref() {
        Some(p) if p.bucket == bucket && p.key == key => slot.take(),
        _ => None,
    }
}
fn keep_partial(bucket: &str, key: &str, etag: Option<String>, total: Option<usize>, have: Vec<u8>) {
    if have.is_empty() {
        return;
    }
    if let Ok(mut slot) = PARTIAL.lock() {
        *slot = Some(Partial { bucket: bucket.to_string(), key: key.to_string(), etag, total, have });
    }
}

/// How long a resume keeps waiting for the network while it brings nothing, and the steps it waits in. A TUNA circuit
/// takes minutes to rebuild after a drop, so three immediate retries always lost the race.
#[cfg(not(test))]
const PATIENCE_SECS: u64 = 600;
#[cfg(not(test))]
const BACKOFF_SECS: [u64; 6] = [2, 5, 10, 20, 30, 60];
// tests run the same paths without the waiting: no patience (a fruitless resume gives up at once, keeping its bytes)
#[cfg(test)]
const PATIENCE_SECS: u64 = 0;
#[cfg(test)]
const BACKOFF_SECS: [u64; 1] = [0];

/// Sleep `secs` while still calling `progress` (the app's pump) every 100 ms, so the app keeps answering its readiness
/// checks while it waits - an app that stops answering is retired by the manager (see fetch_images).
fn wait_pumping(secs: u64, have: usize, total: Option<usize>, progress: &mut dyn FnMut(usize, usize)) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        progress(have, total.unwrap_or(0));
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// GET that survives a connection cut mid-body. NucBox apps reach R2 through a privacy circuit that drops long
/// downloads (2026-10-08: the RISC Box image failed with "peer closed connection" again and again); starting over each
/// time never finishes a big object. So a cut is resumed with `Range: bytes=<have>-` (and `If-Range: <etag>`, so a
/// changed object comes back whole instead of spliced), and appended. While resumes bring nothing it waits with backoff
/// for up to PATIENCE_SECS; if it still gives up, the bytes in hand are kept for the next call for the same object.
/// Returns (status, body) like `request`: a non-200 first answer is the caller's to judge.
fn get_resumable(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(u16, Vec<u8>), String> {
    let (mut have, mut total, mut etag) = match take_partial(bucket, key) {
        Some(p) => {
            eprintln!("[risc-box] s3 {key}: resuming at {} bytes kept from an earlier attempt", p.have.len());
            (p.have, p.total, p.etag)
        }
        None => (Vec::new(), None, None),
    };
    let mut fruitless = 0usize;
    let mut waited = 0u64;
    loop {
        let from = have.len();
        let attempt = {
            let known = total;
            let mut p = |got: usize, cl: usize| progress(from + got, known.unwrap_or(if cl > 0 { from + cl } else { 0 }));
            fetch_raw("GET", ep, bucket, key, creds, &[], from, if from > 0 { etag.as_deref() } else { None }, &mut p)
        };
        let raw = match attempt {
            Ok(r) => r,
            // a resume that cannot even connect: the circuit is down; wait for it (pumping), keeping what we have
            Err(e) if from > 0 => {
                if waited >= PATIENCE_SECS {
                    let msg = format!("resume of {key} at {from} failed for {waited}s ({e}); {from} bytes kept for the next attempt");
                    keep_partial(bucket, key, etag, total, have);
                    return Err(msg);
                }
                let d = BACKOFF_SECS[fruitless.min(BACKOFF_SECS.len() - 1)];
                fruitless += 1;
                eprintln!("[risc-box] s3 {key}: resume at {from} failed ({e}); trying again in {d}s");
                wait_pumping(d, from, total, progress);
                waited += d;
                continue;
            }
            Err(e) => return Err(e),
        };
        // a first answer, or a resume the store answered WHOLE (If-Range: the object changed since `etag`)
        if from == 0 || raw.status == 200 {
            if raw.status != 200 {
                return match raw.cut {
                    Some(why) => Err(why),
                    None => Ok((raw.status, raw.body)),
                };
            }
            if from > 0 {
                eprintln!("[risc-box] s3 {key}: the object changed since byte {from} (If-Range); starting it over");
            }
            total = raw.content_length;
            etag = raw.etag.clone();
            have = raw.body;
        } else {
            if raw.status != 206 || raw.range_start != Some(from) {
                return Err(format!(
                    "resume of {key} at byte {from}: S3 answered {} (range start {:?}); not splicing it",
                    raw.status, raw.range_start
                ));
            }
            if let Some(t) = total {
                have.reserve_exact(t.saturating_sub(have.len()));
            }
            have.extend_from_slice(&raw.body);
        }
        // an attempt that brought bytes resets the patience; one that brought nothing waits before the next
        let brought = have.len() > from || (from > 0 && raw.status == 200);
        if brought {
            fruitless = 0;
            waited = 0;
        }
        if let Some(t) = total {
            if have.len() > t {
                return Err(format!("{key}: got {} bytes of a {t}-byte object", have.len()));
            }
        }
        match raw.cut {
            None => return Ok((200, have)),
            Some(why) => {
                if !brought {
                    if waited >= PATIENCE_SECS {
                        let msg = format!("{why} (resumes brought nothing for {waited}s; {} bytes kept for the next attempt)", have.len());
                        keep_partial(bucket, key, etag, total, have);
                        return Err(msg);
                    }
                    let d = BACKOFF_SECS[fruitless.min(BACKOFF_SECS.len() - 1)];
                    fruitless += 1;
                    wait_pumping(d, have.len(), total, progress);
                    waited += d;
                }
                eprintln!(
                    "[risc-box] s3 {key}: connection cut after {} of {} bytes ({why}); resuming from there",
                    have.len(),
                    total.map_or("?".to_string(), |t| t.to_string())
                );
            }
        }
    }
}

/// Minimal HTTP/1.1 chunked-body decoder.
fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let pos = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("chunked: missing size line")?;
        let size_line = std::str::from_utf8(&raw[..pos]).map_err(|_| "chunked: bad size")?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| "chunked: bad size hex")?;
        raw = &raw[pos + 2..];
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < size + 2 {
            return Err("chunked: truncated".into());
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

fn s3_error(status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(400)]);
    format!("S3 answered {status}: {text}")
}

pub fn get_object(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Vec<u8>, String> {
    let (status, body) = get_resumable(ep, bucket, key, creds, progress)?;
    if status != 200 {
        return Err(s3_error(status, &body));
    }
    Ok(body)
}

/// GET that treats a missing object as an answer rather than an error:
/// `Ok(None)` on 404. A snapshot key is configured before any snapshot
/// exists under it, and that first cold boot is not a failure.
pub fn get_object_opt(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Option<Vec<u8>>, String> {
    let (status, body) = get_resumable(ep, bucket, key, creds, progress)?;
    match status {
        200 => Ok(Some(body)),
        404 => Ok(None),
        _ => Err(s3_error(status, &body)),
    }
}

pub fn put_object(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    body: &[u8],
) -> Result<(), String> {
    let (status, resp) = request("PUT", ep, bucket, key, creds, body, &mut |_, _| {})?;
    if status != 200 {
        return Err(s3_error(status, &resp));
    }
    Ok(())
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A scripted plain-HTTP object server: `script(n, request_head)` answers connection n with raw bytes (or `None`:
    /// close without answering). Returns the endpoint and a channel of every request head it saw.
    fn serve(script: impl Fn(usize, &str) -> Option<Vec<u8>> + Send + 'static, conns: usize) -> (Endpoint, mpsc::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for n in 0..conns {
                let Ok((mut s, _)) = l.accept() else { return };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                tx.send(head.clone()).ok();
                if let Some(bytes) = script(n, &head) {
                    s.write_all(&bytes).ok();
                }
            }
        });
        (Endpoint { host: "127.0.0.1".into(), port, https: false, region: "auto".into() }, rx)
    }
    fn object(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }
    fn ok200(body: &[u8], etag: &str, send: usize) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\netag: {etag}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body[..send]);
        out
    }
    fn partial206(body: &[u8], from: usize, etag: &str) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 206 Partial Content\r\ncontent-length: {}\r\ncontent-range: bytes {from}-{}/{}\r\netag: {etag}\r\n\r\n",
            body.len() - from, body.len() - 1, body.len()).into_bytes();
        out.extend_from_slice(&body[from..]);
        out
    }

    #[test]
    fn giving_up_keeps_the_bytes_and_the_next_call_resumes_from_them() {
        let obj = object(200_000, 7);
        let o = obj.clone();
        // conn 0: 40% then the connection drops; conn 1: refused (the circuit is down); conn 2: the rest
        let (ep, heads) = serve(move |n, head| match n {
            0 => Some(ok200(&o, "\"v1\"", 80_000)),
            1 => None,
            _ => {
                assert!(head.contains("range: bytes=80000-"), "resumed from the kept bytes: {head}");
                assert!(head.contains("if-range: \"v1\""), "with the object's etag: {head}");
                Some(partial206(&o, 80_000, "\"v1\""))
            }
        }, 3);
        let mut nop = |_: usize, _: usize| {};
        let first = get_resumable(&ep, "b", "disk.img", None, &mut nop);
        assert!(first.is_err(), "the circuit stayed down past the patience: {first:?}");
        let (status, body) = get_resumable(&ep, "b", "disk.img", None, &mut nop).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, obj, "the kept 80 000 bytes and the resumed rest are the object, byte for byte");
        let seen: Vec<String> = heads.try_iter().collect();
        assert_eq!(seen.len(), 3);
        assert!(!seen[0].contains("range:"), "the first request asks for the whole object");
    }

    #[test]
    fn a_changed_object_is_started_over_not_spliced() {
        let old = object(100_000, 1);
        let new = object(120_000, 9);
        let (o, n2) = (old.clone(), new.clone());
        // conn 0: half of v1, then a drop; conn 1: the store answers the If-Range resume WHOLE (v2: the object changed)
        let (ep, _heads) = serve(move |n, _| match n {
            0 => Some(ok200(&o, "\"v1\"", 50_000)),
            _ => Some(ok200(&n2, "\"v2\"", n2.len())),
        }, 2);
        let mut nop = |_: usize, _: usize| {};
        let (status, body) = get_resumable(&ep, "b", "changed.img", None, &mut nop).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, new, "the new object whole, never half of v1 + the tail of v2");
    }

    #[test]
    fn a_cut_that_brought_bytes_resumes_at_once() {
        let obj = object(90_000, 3);
        let o = obj.clone();
        let (ep, _heads) = serve(move |n, _| match n {
            0 => Some(ok200(&o, "\"e\"", 30_000)),
            1 => {
                let mut r = partial206(&o, 30_000, "\"e\"");
                r.truncate(r.len() - 30_000); // brings 30 000 more, then drops again
                Some(r)
            }
            _ => Some(partial206(&o, 60_000, "\"e\"")),
        }, 3);
        let mut nop = |_: usize, _: usize| {};
        let (_, body) = get_resumable(&ep, "b", "cuts.img", None, &mut nop).unwrap();
        assert_eq!(body, obj);
    }
}
