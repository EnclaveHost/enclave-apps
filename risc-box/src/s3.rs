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
    let sock = TcpStream::connect((ep.host.as_str(), ep.port))
        .map_err(|e| format!("connect {}:{}: {e}", ep.host, ep.port))?;
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
                            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                                chunked = true;
                            }
                        }
                    }
                }
                if let (Some(he), Some(cl)) = (head_end, content_length) {
                    progress(rbuf.len() - he, cl);
                    if rbuf.len() >= he + cl {
                        break;
                    }
                } else if head_end.is_some() {
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
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let he = head_end.ok_or("response ended before headers completed")?;
    let raw = &rbuf[he..];
    let body = if chunked { dechunk(raw)? } else { raw.to_vec() };
    if let Some(cl) = content_length {
        if body.len() < cl {
            return Err(format!("short body: {} of {cl} bytes", body.len()));
        }
    }
    Ok((status, body))
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
    let (status, body) = request("GET", ep, bucket, key, creds, &[], progress)?;
    if status != 200 {
        return Err(s3_error(status, &body));
    }
    Ok(body)
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
