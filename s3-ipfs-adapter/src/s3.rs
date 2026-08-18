//! Minimal S3 client, extended from risc-box's: LIST the bucket
//! (ListObjectsV2, paginated) and GET objects by byte range, over the
//! platform's transparent egress.
//!
//! - `https://` endpoints use rustls with the pure-Rust RustCrypto provider,
//!   the only TLS stack that builds for wasm32-wasip2, with webpki roots.
//!   `http://` endpoints use a plain TcpStream (for local mocks/minio).
//! - Requests are path-style (`/bucket/key`), which every S3-compatible
//!   store accepts and which keeps TLS SNI independent of bucket names.
//! - With credentials, requests are SigV4-signed; the canonical query string
//!   and the Range header are part of the signature. Without credentials,
//!   requests go unsigned: public buckets and mock servers.
//!
//! No chrono, no aws-sdk: the date math and the signing chain are a page of
//! std + sha2/hmac.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Wall-clock ceiling for one request (connect + TLS + transfer). Windows
/// are at most a few MiB, so a request that can't finish in this long is a
/// wedged connection, not a slow one; erroring lets the caller retry fresh.
const REQUEST_DEADLINE: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

#[derive(Clone)]
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

/// RFC 3986 URI-encode. SigV4's canonical form: unreserved bytes bare,
/// everything else percent-encoded; slashes preserved only in object keys.
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The canonical (and wire) query string: keys sorted, both sides encoded.
fn query_string(query: &[(String, String)]) -> String {
    let mut pairs: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs.join("&")
}

/// Build the SigV4 Authorization + x-amz-* headers for a request.
/// `extra` headers (e.g. range) are included in the signature.
fn sign(
    method: &str,
    ep: &Endpoint,
    canonical_uri: &str,
    canonical_query: &str,
    payload_hash: &str,
    extra: &[(String, String)],
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
    headers.extend(extra.iter().cloned());
    headers.sort();
    let signed_names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let signed_list = signed_names.join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_list}\n{payload_hash}"
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
    // No connect timeout, deliberately: dial(None) is plain TcpStream::connect,
    // which resolves names and tries each address itself. connect_timeout is
    // not trustworthy on wasm32-wasip2 (it can hand back a socket whose
    // connect never completed; the failure then surfaces as ENOTCONN on the
    // first write). Post-connect, the read timeout below and the caller's
    // REQUEST_DEADLINE bound every request.
    let sock = crate::egress::dial(ep.host.as_str(), ep.port, None)?;
    // A read timeout so a wedged peer surfaces as an error instead of
    // stalling the (single-threaded) event loop forever.
    let _ = sock.set_read_timeout(Some(Duration::from_secs(30)));
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

/// One S3 request. GETs pass an empty `body`; PUTs sign and send theirs
/// (the payload hash of an empty body IS the empty-body hash, so every
/// method takes the same path). Returns (status, response body).
fn request(
    method: &str,
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    query: &[(String, String)],
    range: Option<(u64, u64)>, // inclusive byte range
    creds: Option<&Creds>,
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let canonical_uri = if key.is_empty() {
        format!("/{}", uri_encode(bucket, true))
    } else {
        format!("/{}/{}", uri_encode(bucket, true), uri_encode(key, true))
    };
    let canonical_query = query_string(query);
    let payload_hash = if body.is_empty() {
        EMPTY_SHA256.to_string()
    } else {
        hex(&Sha256::digest(body))
    };
    let host_header = if (ep.https && ep.port == 443) || (!ep.https && ep.port == 80) {
        ep.host.clone()
    } else {
        format!("{}:{}", ep.host, ep.port)
    };
    let target = if canonical_query.is_empty() {
        canonical_uri.clone()
    } else {
        format!("{canonical_uri}?{canonical_query}")
    };
    let extra: Vec<(String, String)> = range
        .map(|(a, b)| vec![("range".to_string(), format!("bytes={a}-{b}"))])
        .unwrap_or_default();
    let mut head = format!("{method} {target} HTTP/1.1\r\nhost: {host_header}\r\n");
    match creds {
        Some(c) => {
            for (k, v) in sign(method, ep, &canonical_uri, &canonical_query, &payload_hash, &extra, c) {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        None => {
            for (k, v) in &extra {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            // unsigned PUT (public bucket / mock): still send the content
            // hash, some stores want it
            if method == "PUT" {
                head.push_str(&format!("x-amz-content-sha256: {payload_hash}\r\n"));
            }
        }
    }
    head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", body.len()));

    let deadline = Instant::now() + REQUEST_DEADLINE;
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
        if Instant::now() > deadline {
            return Err("request deadline exceeded".into());
        }
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
                    if rbuf.len() >= he + cl {
                        break;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err("read timeout".into());
            }
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

/// GET `len` bytes of an object starting at `start`. The store may clamp the
/// range at EOF, so the result can be shorter than asked; never longer.
pub fn get_range(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    start: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let (status, mut body) =
        request("GET", ep, bucket, key, &[], Some((start, start + len - 1)), creds, &[])?;
    match status {
        206 => Ok(body),
        // A store that ignores Range answers 200 with the whole object.
        200 if start == 0 => {
            body.truncate(len as usize);
            Ok(body)
        }
        200 => Err("store ignored the Range header".into()),
        _ => Err(s3_error(status, &body)),
    }
}

pub struct ObjMeta {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

/// PUT one object. S3 answers 200 with an ETag header.
pub fn put_object(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
    body: &[u8],
) -> Result<(), String> {
    let (status, resp) = request("PUT", ep, bucket, key, &[], None, creds, body)?;
    if status != 200 {
        return Err(s3_error(status, &resp));
    }
    Ok(())
}

/// DELETE one object. S3 answers 204 (idempotent: also for absent keys).
pub fn delete_object(
    ep: &Endpoint,
    bucket: &str,
    key: &str,
    creds: Option<&Creds>,
) -> Result<(), String> {
    let (status, resp) = request("DELETE", ep, bucket, key, &[], None, creds, &[])?;
    if !matches!(status, 200 | 202 | 204) {
        return Err(s3_error(status, &resp));
    }
    Ok(())
}

/// One page of ListObjectsV2. Returns (objects, next continuation token).
pub fn list_page(
    ep: &Endpoint,
    bucket: &str,
    prefix: &str,
    cont: Option<&str>,
    creds: Option<&Creds>,
) -> Result<(Vec<ObjMeta>, Option<String>), String> {
    let mut query = vec![
        ("list-type".to_string(), "2".to_string()),
        ("max-keys".to_string(), "1000".to_string()),
    ];
    if !prefix.is_empty() {
        query.push(("prefix".to_string(), prefix.to_string()));
    }
    if let Some(c) = cont {
        query.push(("continuation-token".to_string(), c.to_string()));
    }
    let (status, body) = request("GET", ep, bucket, "", &query, None, creds, &[])?;
    if status != 200 {
        return Err(s3_error(status, &body));
    }
    let xml = String::from_utf8_lossy(&body);
    let mut objects = Vec::new();
    for contents in xml_blocks(&xml, "Contents") {
        let Some(key) = xml_field(contents, "Key").map(|s| xml_unescape(&s)) else { continue };
        let size = xml_field(contents, "Size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let etag = xml_field(contents, "ETag")
            .map(|s| xml_unescape(&s).trim_matches('"').to_string())
            .unwrap_or_default();
        objects.push(ObjMeta { key, size, etag });
    }
    let truncated = xml_field(&xml, "IsTruncated").as_deref() == Some("true");
    let next = if truncated {
        xml_field(&xml, "NextContinuationToken").map(|s| xml_unescape(&s))
    } else {
        None
    };
    Ok((objects, next))
}

/// The inner spans of every `<tag>...</tag>` block, in order.
fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(a) = rest.find(&open) {
        let inner = &rest[a + open.len()..];
        let Some(b) = inner.find(&close) else { break };
        out.push(&inner[..b]);
        rest = &inner[b + close.len()..];
    }
    out
}

fn xml_field(xml: &str, tag: &str) -> Option<String> {
    xml_blocks(xml, tag).first().map(|s| s.to_string())
}

/// The five XML entities plus numeric character references; S3 escapes keys
/// with exactly these.
fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find('&') {
        out.push_str(&rest[..a]);
        rest = &rest[a..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let ent = &rest[1..semi];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = ent
                    .strip_prefix("#x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()));
                match code.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[..semi + 1]),
                }
            }
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_xml_parsing() {
        let xml = r#"<?xml version="1.0"?><ListBucketResult>
            <IsTruncated>true</IsTruncated>
            <Contents><Key>a/b &amp; c.txt</Key><LastModified>x</LastModified><ETag>&quot;abc123&quot;</ETag><Size>42</Size></Contents>
            <Contents><Key>plain.bin</Key><ETag>"def"</ETag><Size>0</Size></Contents>
            <NextContinuationToken>tok+1=</NextContinuationToken>
            </ListBucketResult>"#;
        let blocks = xml_blocks(xml, "Contents");
        assert_eq!(blocks.len(), 2);
        assert_eq!(xml_unescape(&xml_field(blocks[0], "Key").unwrap()), "a/b & c.txt");
        assert_eq!(
            xml_field(blocks[0], "ETag").map(|s| xml_unescape(&s).trim_matches('"').to_string()),
            Some("abc123".into())
        );
        assert_eq!(xml_field(&xml, "NextContinuationToken").as_deref(), Some("tok+1="));
        assert_eq!(xml_unescape("x&#x41;&#66;&bogus;y"), "xAB&bogus;y");
    }

    #[test]
    fn canonical_query() {
        let q = vec![
            ("prefix".to_string(), "a b/c".to_string()),
            ("list-type".to_string(), "2".to_string()),
        ];
        assert_eq!(query_string(&q), "list-type=2&prefix=a%20b%2Fc");
    }
}
