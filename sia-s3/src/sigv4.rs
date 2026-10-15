//! AWS Signature V4 verification (header form) for incoming S3 requests.
//!
//! The canonical request is rebuilt from the bytes the client sent: the path
//! verbatim (S3 does not double-encode), the query re-normalised, the signed
//! headers, and the client's `x-amz-content-sha256`. When that header is a hex
//! digest the body must hash to it; `UNSIGNED-PAYLOAD` is accepted (the request
//! line and headers stay signed). Streaming (aws-chunked) payloads are refused.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::http::Request;

type HmacSha256 = Hmac<Sha256>;

const MAX_SKEW_SECS: i64 = 15 * 60;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac takes any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// AWS URI encoding: unreserved characters stay, everything else is %XX.
pub fn aws_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        // '+' stays literal: S3 clients encode spaces as %20
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Query pairs, decoded.
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

fn canonical_query(raw: &str) -> String {
    let mut pairs: Vec<(String, String)> = parse_query(raw)
        .into_iter()
        .map(|(k, v)| (aws_encode(&k), aws_encode(&v)))
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub enum Verdict {
    Ok,
    Denied(&'static str, String),
}

/// Parse `AWS4-HMAC-SHA256 Credential=AK/DATE/REGION/s3/aws4_request, SignedHeaders=a;b, Signature=hex`.
fn parse_auth(v: &str) -> Option<(String, String, String, String, Vec<String>, String)> {
    let rest = v.strip_prefix("AWS4-HMAC-SHA256")?.trim();
    let (mut cred, mut signed, mut sig) = (None, None, None);
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(c) = part.strip_prefix("Credential=") {
            cred = Some(c.to_string());
        } else if let Some(s) = part.strip_prefix("SignedHeaders=") {
            signed = Some(s.split(';').map(|h| h.to_ascii_lowercase()).collect::<Vec<_>>());
        } else if let Some(s) = part.strip_prefix("Signature=") {
            sig = Some(s.to_ascii_lowercase());
        }
    }
    let cred = cred?;
    let mut it = cred.split('/');
    let (ak, date, region, service, term) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next()?);
    if service != "s3" || term != "aws4_request" {
        return None;
    }
    Some((ak.to_string(), date.to_string(), region.to_string(), service.to_string(), signed?, sig?))
}

pub fn verify(req: &Request, credentials: &[(String, String)]) -> Verdict {
    let Some(auth) = req.header("authorization") else {
        return Verdict::Denied("AccessDenied", "requests must be signed (SigV4)".into());
    };
    let Some((ak, date, region, _svc, signed, sig)) = parse_auth(auth) else {
        return Verdict::Denied("AuthorizationHeaderMalformed", "unsupported Authorization header".into());
    };
    let Some(secret) = credentials.iter().find(|(id, _)| *id == ak).map(|(_, s)| s) else {
        return Verdict::Denied("InvalidAccessKeyId", "unknown access key".into());
    };
    let Some(amz_date) = req.header("x-amz-date") else {
        return Verdict::Denied("AccessDenied", "missing x-amz-date".into());
    };
    let Ok(when) = chrono::NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ") else {
        return Verdict::Denied("AccessDenied", "bad x-amz-date".into());
    };
    if (chrono::Utc::now().naive_utc() - when).num_seconds().abs() > MAX_SKEW_SECS {
        return Verdict::Denied("RequestTimeTooSkewed", "x-amz-date is more than 15 minutes off".into());
    }
    if !amz_date.starts_with(&date) {
        return Verdict::Denied("AccessDenied", "credential date does not match x-amz-date".into());
    }
    let payload = req.header("x-amz-content-sha256").unwrap_or("");
    match payload {
        "UNSIGNED-PAYLOAD" => {}
        // a streamed body is hashed as it arrives and compared at its end (s3.rs), before anything is committed
        p if p.len() == 64 && p.bytes().all(|c| c.is_ascii_hexdigit()) => {
            if req.stream.is_none() && hex::encode(Sha256::digest(&req.body)) != p.to_ascii_lowercase() {
                return Verdict::Denied("XAmzContentSHA256Mismatch", "body does not match x-amz-content-sha256".into());
            }
        }
        _ => return Verdict::Denied("NotImplemented", "x-amz-content-sha256 must be a hex digest or UNSIGNED-PAYLOAD (no aws-chunked)".into()),
    }
    if !signed.iter().any(|h| h == "host") {
        return Verdict::Denied("AccessDenied", "host must be signed".into());
    }

    let mut canon_headers = String::new();
    for h in &signed {
        let v = req.header(h).unwrap_or("");
        let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
        canon_headers.push_str(&format!("{h}:{v}\n"));
    }
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.path,
        canonical_query(&req.query),
        canon_headers,
        signed.join(";"),
        payload
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    let expect = hex::encode(hmac(&k, to_sign.as_bytes()));
    // constant-time compare
    let ok = expect.len() == sig.len() && expect.bytes().zip(sig.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0;
    if ok {
        Verdict::Ok
    } else {
        Verdict::Denied("SignatureDoesNotMatch", "signature does not match".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_aws() {
        assert_eq!(aws_encode("a b/c~d"), "a%20b%2Fc~d");
        assert_eq!(canonical_query("uploads"), "uploads=");
        assert_eq!(canonical_query("prefix=pins%2F&list-type=2"), "list-type=2&prefix=pins%2F");
    }
}
