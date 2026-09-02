//! A minimal S3 client on top of wasi:http: GET, PUT, DELETE one object and
//! LIST a prefix, SigV4-signed when credentials are configured. The signing
//! chain is the one risc-box and s3-ipfs-adapter carry (a page of std +
//! sha2/hmac); what differs here is the transport. Those two apps open their
//! own sockets and run rustls in the guest; this one hands the request to
//! wasi:http/outgoing-handler and lets the host do TLS, which is smaller,
//! and is what a `wasi:http` component has anyway.
//!
//! The one consequence of that choice: the guest cannot set the `host`
//! header (forbidden by contract), so the signature is computed over the
//! authority the host WILL send. `Endpoint::parse` normalises the authority
//! (default port dropped) so the two agree.
//!
//! Requests are path-style (`/bucket/key`): every S3-compatible store accepts
//! it and it keeps the TLS name independent of bucket names.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::bindings::wasi::http::types::Method;
use crate::http::{self, HttpReq};

#[derive(Clone)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

#[derive(Clone)]
pub struct Endpoint {
    pub https: bool,
    /// host[:port], the `host` header value the host runtime will send
    pub authority: String,
    pub region: String,
}

impl Endpoint {
    /// Parse "https://<account>.r2.cloudflarestorage.com" / "http://127.0.0.1:9000".
    pub fn parse(url: &str, region: &str) -> Result<Endpoint, String> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("endpoint must be http(s)://host[:port], got {url}"));
        };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') {
            return Err("endpoint must be scheme://host[:port] with no path".into());
        }
        let authority = match rest.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                let port: u16 = p.parse().map_err(|_| "bad port in endpoint")?;
                if (https && port == 443) || (!https && port == 80) {
                    h.to_string()
                } else {
                    format!("{h}:{port}")
                }
            }
            _ => rest.to_string(),
        };
        Ok(Endpoint { https, authority, region: region.to_string() })
    }
}

pub enum S3Error {
    /// the store answered, with this status (and the head of its body)
    Http(u16, String),
    /// nothing came back: egress, TLS, timeout
    Transport(String),
}

impl S3Error {
    pub fn message(&self) -> String {
        match self {
            S3Error::Http(status, text) => format!("S3 answered {status}: {text}"),
            S3Error::Transport(e) => e.clone(),
        }
    }
}

pub struct Object {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub modified: String,
}

pub struct Fetched {
    pub body: Vec<u8>,
    pub etag: String,
    pub modified: String,
}

pub struct Client<'a> {
    pub ep: &'a Endpoint,
    pub bucket: &'a str,
    pub creds: Option<&'a Creds>,
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// one object, as read: a note is capped at 1 MiB on write, so anything
/// bigger under the prefix was not written by this app
pub const MAX_OBJECT: usize = 4 * 1024 * 1024;
/// a LIST page (1000 keys of XML)
const MAX_LIST_XML: usize = 2 * 1024 * 1024;

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
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
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

/// The SigV4 Authorization + x-amz-* headers for a request. `extra` headers
/// (if-match, content-type) are part of the signature and go on the wire
/// with exactly the signed values.
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
    let mut headers = vec![
        ("host".to_string(), ep.authority.clone()),
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
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_list}\n{payload_hash}"
    );
    let scope = format!("{date}/{}/s3/aws4_request", ep.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let k_date = hmac_sha256(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, ep.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_list}, Signature={signature}",
        creds.access_key_id
    );
    // host is the runtime's to send; everything else goes out verbatim
    headers.retain(|(k, _)| k != "host");
    headers.push(("authorization".to_string(), auth));
    headers
}

fn etag_of(resp: &http::Response) -> String {
    resp.header("etag").map(|v| v.trim_matches('"').to_string()).unwrap_or_default()
}

impl<'a> Client<'a> {
    fn request(
        &self,
        method: Method,
        method_name: &str,
        key: &str,
        query: &[(String, String)],
        body: &[u8],
        extra: &[(String, String)],
        max_bytes: usize,
    ) -> Result<http::Response, S3Error> {
        let canonical_uri = if key.is_empty() {
            format!("/{}", uri_encode(self.bucket, true))
        } else {
            format!("/{}/{}", uri_encode(self.bucket, true), uri_encode(key, true))
        };
        let canonical_query = query_string(query);
        let payload_hash =
            if body.is_empty() { EMPTY_SHA256.to_string() } else { hex(&Sha256::digest(body)) };
        let target = if canonical_query.is_empty() {
            canonical_uri.clone()
        } else {
            format!("{canonical_uri}?{canonical_query}")
        };
        let headers: Vec<(String, String)> = match self.creds {
            Some(c) => sign(
                method_name,
                self.ep,
                &canonical_uri,
                &canonical_query,
                &payload_hash,
                extra,
                c,
            ),
            None => {
                // unsigned (public bucket / mock): still send the content
                // hash, some stores want it on a PUT
                let mut h = extra.to_vec();
                h.push(("x-amz-content-sha256".to_string(), payload_hash));
                h
            }
        };
        http::request(&HttpReq {
            method,
            https: self.ep.https,
            authority: &self.ep.authority,
            path_with_query: &target,
            headers: &headers,
            body,
            timeout_s: 30,
            max_bytes,
        })
        .map_err(S3Error::Transport)
    }

    /// GET one object: None when the store says 404.
    pub fn get(&self, key: &str) -> Result<Option<Fetched>, S3Error> {
        let r = self.request(Method::Get, "GET", key, &[], &[], &[], MAX_OBJECT)?;
        match r.status {
            200 => Ok(Some(Fetched {
                etag: etag_of(&r),
                modified: r.header("last-modified").unwrap_or("").to_string(),
                body: r.body,
            })),
            404 => Ok(None),
            s => Err(S3Error::Http(s, body_head(&r.body))),
        }
    }

    /// PUT one object; `if_match` makes it conditional on the current ETag
    /// (412 comes back as S3Error::Http(412, ..)). Returns the new ETag.
    pub fn put(
        &self,
        key: &str,
        body: &[u8],
        content_type: &str,
        if_match: Option<&str>,
    ) -> Result<String, S3Error> {
        let mut extra = vec![("content-type".to_string(), content_type.to_string())];
        if let Some(tag) = if_match {
            extra.push(("if-match".to_string(), format!("\"{}\"", tag.trim_matches('"'))));
        }
        let r = self.request(Method::Put, "PUT", key, &[], body, &extra, 64 * 1024)?;
        match r.status {
            200 => Ok(etag_of(&r)),
            s => Err(S3Error::Http(s, body_head(&r.body))),
        }
    }

    /// DELETE one object. S3 answers 204, also for keys that never existed.
    pub fn delete(&self, key: &str) -> Result<(), S3Error> {
        let r = self.request(Method::Delete, "DELETE", key, &[], &[], &[], 64 * 1024)?;
        match r.status {
            200 | 202 | 204 => Ok(()),
            s => Err(S3Error::Http(s, body_head(&r.body))),
        }
    }

    /// Up to `max` objects under `prefix` (ListObjectsV2, paginated).
    /// Returns (objects, more-were-available).
    pub fn list(&self, prefix: &str, max: usize) -> Result<(Vec<Object>, bool), S3Error> {
        let mut out = Vec::new();
        let mut cont: Option<String> = None;
        loop {
            let page = (max - out.len()).clamp(1, 1000);
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("max-keys".to_string(), page.to_string()),
            ];
            if !prefix.is_empty() {
                query.push(("prefix".to_string(), prefix.to_string()));
            }
            if let Some(c) = &cont {
                query.push(("continuation-token".to_string(), c.clone()));
            }
            let r = self.request(Method::Get, "GET", "", &query, &[], &[], MAX_LIST_XML)?;
            if r.status != 200 {
                return Err(S3Error::Http(r.status, body_head(&r.body)));
            }
            let xml = String::from_utf8_lossy(&r.body);
            for contents in xml_blocks(&xml, "Contents") {
                let Some(key) = xml_field(contents, "Key").map(|s| xml_unescape(&s)) else { continue };
                out.push(Object {
                    key,
                    size: xml_field(contents, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    etag: xml_field(contents, "ETag")
                        .map(|s| xml_unescape(&s).trim_matches('"').to_string())
                        .unwrap_or_default(),
                    modified: xml_field(contents, "LastModified").unwrap_or_default(),
                });
            }
            let truncated = xml_field(&xml, "IsTruncated").as_deref() == Some("true");
            if !truncated {
                return Ok((out, false));
            }
            if out.len() >= max {
                return Ok((out, true));
            }
            cont = xml_field(&xml, "NextContinuationToken").map(|s| xml_unescape(&s));
            if cont.is_none() {
                return Ok((out, true));
            }
        }
    }
}

fn body_head(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(400)]);
    // S3 error bodies are XML; the <Message> is the part worth reading
    xml_field(&text, "Message").map(|m| xml_unescape(&m)).unwrap_or_else(|| text.trim().to_string())
}

/// The inner spans of every `<tag>...</tag>` block, in order.
fn xml_blocks<'x>(xml: &'x str, tag: &str) -> Vec<&'x str> {
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
    fn endpoint_authority_matches_what_the_host_sends() {
        assert_eq!(Endpoint::parse("https://x.r2.cloudflarestorage.com", "auto").unwrap().authority, "x.r2.cloudflarestorage.com");
        assert_eq!(Endpoint::parse("https://x.example:443/", "auto").unwrap().authority, "x.example");
        assert_eq!(Endpoint::parse("http://127.0.0.1:9000", "us-east-1").unwrap().authority, "127.0.0.1:9000");
        assert!(Endpoint::parse("x.example", "auto").is_err());
        assert!(Endpoint::parse("https://x.example/bucket", "auto").is_err());
    }

    #[test]
    fn canonical_query_and_list_xml() {
        let q = vec![
            ("prefix".to_string(), "a b/c".to_string()),
            ("list-type".to_string(), "2".to_string()),
        ];
        assert_eq!(query_string(&q), "list-type=2&prefix=a%20b%2Fc");
        let xml = r#"<ListBucketResult><IsTruncated>true</IsTruncated>
            <Contents><Key>notes/a &amp; b.md</Key><LastModified>2026-09-01T00:00:00.000Z</LastModified><ETag>&quot;abc&quot;</ETag><Size>42</Size></Contents>
            <NextContinuationToken>tok+1=</NextContinuationToken></ListBucketResult>"#;
        let blocks = xml_blocks(xml, "Contents");
        assert_eq!(blocks.len(), 1);
        assert_eq!(xml_unescape(&xml_field(blocks[0], "Key").unwrap()), "notes/a & b.md");
        assert_eq!(xml_field(&xml, "NextContinuationToken").as_deref(), Some("tok+1="));
    }
}
