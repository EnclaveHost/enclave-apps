//! The S3 calls depot makes (R2 is the target; any S3-compatible store with
//! conditional PUT works): GET with Range and If-None-Match, PUT with
//! If-Match / If-None-Match, DELETE, ListObjectsV2, multipart upload.
//! Path-style requests, SigV4 over every header we send. Transient failures
//! (connection errors, 5xx, 429) are retried with backoff.

use crate::client::{Client, Response};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub struct S3 {
    client: Client,
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    pub calls: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

pub enum Cond<'a> {
    None,
    IfMatch(&'a str),
    IfNoneMatch,
}

fn hex(b: &[u8]) -> String {
    crate::git::hex(b)
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
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

/// (YYYYMMDD, YYYYMMDDTHHMMSSZ) from the system clock, civil-from-days.
fn amz_dates() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (y, mo, d) = civil(secs.div_euclid(86400));
    let sod = secs.rem_euclid(86400);
    let date = format!("{y:04}{mo:02}{d:02}");
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    );
    (date, stamp)
}

pub fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if mo <= 2 { 1 } else { 0 };
    (y, mo, d)
}

pub struct Got {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
}

fn transient(status: u16) -> bool {
    status == 429 || status >= 500
}

impl S3 {
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<S3, String> {
        Ok(S3 {
            client: Client::new(endpoint)?,
            region: region.to_string(),
            bucket: bucket.to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            calls: 0,
            bytes_in: 0,
            bytes_out: 0,
        })
    }

    pub fn connects(&self) -> u64 {
        self.client.connects
    }

    fn sign(
        &self,
        method: &str,
        uri: &str,
        query: &str,
        payload_hash: &str,
        extra: &[(String, String)],
    ) -> Vec<(String, String)> {
        let (date, stamp) = amz_dates();
        let mut h: Vec<(String, String)> = vec![
            ("host".into(), self.client.authority()),
            ("x-amz-content-sha256".into(), payload_hash.into()),
            ("x-amz-date".into(), stamp.clone()),
        ];
        h.extend(extra.iter().cloned());
        h.sort();
        if self.access_key.is_empty() {
            h.retain(|(k, _)| k != "host");
            return h;
        }
        let names: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
        let signed = names.join(";");
        let canon_headers: String = h
            .iter()
            .map(|(k, v)| format!("{k}:{}\n", v.trim()))
            .collect();
        let creq = format!("{method}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{payload_hash}");
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let sts = format!(
            "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
            hex(&Sha256::digest(creq.as_bytes()))
        );
        let k = hmac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let k = hmac(&k, self.region.as_bytes());
        let k = hmac(&k, b"s3");
        let k = hmac(&k, b"aws4_request");
        let sig = hex(&hmac(&k, sts.as_bytes()));
        h.retain(|(k, _)| k != "host");
        h.push((
            "authorization".into(),
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}",
                self.access_key
            ),
        ));
        h
    }

    fn query(q: &[(&str, &str)]) -> String {
        let mut v: Vec<String> = q
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
            .collect();
        v.sort();
        v.join("&")
    }

    /// One signed request with retries for transient failures.
    fn call(
        &mut self,
        method: &str,
        key: &str,
        q: &[(&str, &str)],
        extra: &[(String, String)],
        body: &[u8],
    ) -> Result<Response, String> {
        let uri = if key.is_empty() {
            format!("/{}", uri_encode(&self.bucket, false))
        } else {
            format!(
                "/{}/{}",
                uri_encode(&self.bucket, false),
                uri_encode(key, true)
            )
        };
        let query = Self::query(q);
        let target = if query.is_empty() {
            uri.clone()
        } else {
            format!("{uri}?{query}")
        };
        let payload = hex(&Sha256::digest(body));
        let mut last = String::new();
        for attempt in 0..5u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250 * (1 << attempt.min(4)) as u64));
            }
            let headers = self.sign(method, &uri, &query, &payload, extra);
            self.calls += 1;
            match self.client.send(method, &target, &headers, body) {
                Ok(r) if transient(r.status) => {
                    last = format!("storage answered HTTP {}", r.status);
                    self.client.drop_connection();
                }
                Ok(r) => {
                    self.bytes_out += body.len() as u64;
                    self.bytes_in += r.body.len() as u64;
                    return Ok(r);
                }
                Err(e) => {
                    last = e;
                    self.client.drop_connection();
                }
            }
        }
        Err(format!("storage unavailable after retries: {last}"))
    }

    fn etag(r: &Response) -> Option<String> {
        r.header("etag")
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    fn error(op: &str, r: &Response) -> String {
        let code = String::from_utf8_lossy(&r.body);
        let code = code
            .split("<Code>")
            .nth(1)
            .and_then(|s| s.split("</Code>").next())
            .unwrap_or("")
            .to_string();
        format!("storage {op} failed: HTTP {} {code}", r.status)
    }

    /// GET a whole object, or `None` when it does not exist. With
    /// `if_none_match`, a 304 comes back as status 304 with no body.
    pub fn get(&mut self, key: &str, if_none_match: Option<&str>) -> Result<Option<Got>, String> {
        let mut extra = Vec::new();
        if let Some(t) = if_none_match {
            extra.push(("if-none-match".to_string(), t.to_string()));
        }
        let r = self.call("GET", key, &[], &extra, &[])?;
        match r.status {
            200 | 304 => Ok(Some(Got {
                status: r.status,
                etag: Self::etag(&r),
                body: r.body,
            })),
            404 => Ok(None),
            _ => Err(Self::error("GET", &r)),
        }
    }

    /// Bytes [start, end) of an object.
    pub fn get_range(&mut self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
        let extra = vec![("range".to_string(), format!("bytes={}-{}", start, end - 1))];
        let r = self.call("GET", key, &[], &extra, &[])?;
        match r.status {
            206 => {
                if r.body.len() as u64 != end - start {
                    return Err("storage returned a short range".into());
                }
                Ok(r.body)
            }
            200 if start == 0 && r.body.len() as u64 >= end => Ok(r.body[..end as usize].to_vec()),
            404 => Err(format!("stored object {key} is missing")),
            _ => Err(Self::error("GET range", &r)),
        }
    }

    /// PUT; `Ok(Err(status))` on a failed precondition (412/409), else the new ETag.
    pub fn put(
        &mut self,
        key: &str,
        body: &[u8],
        cond: Cond,
    ) -> Result<Result<Option<String>, u16>, String> {
        let mut extra = vec![(
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        )];
        match cond {
            Cond::None => {}
            Cond::IfMatch(t) => extra.push(("if-match".into(), t.to_string())),
            Cond::IfNoneMatch => extra.push(("if-none-match".into(), "*".into())),
        }
        let r = self.call("PUT", key, &[], &extra, body)?;
        match r.status {
            200 | 201 | 204 => Ok(Ok(Self::etag(&r))),
            412 | 409 => Ok(Err(r.status)),
            _ => Err(Self::error("PUT", &r)),
        }
    }

    pub fn delete(&mut self, key: &str) -> Result<(), String> {
        let r = self.call("DELETE", key, &[], &[], &[])?;
        match r.status {
            200 | 202 | 204 | 404 => Ok(()),
            _ => Err(Self::error("DELETE", &r)),
        }
    }

    /// One page of ListObjectsV2: (keys with sizes, continuation token).
    pub fn list(
        &mut self,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<(Vec<(String, u64)>, Option<String>), String> {
        let mut q = vec![("list-type", "2"), ("max-keys", "1000"), ("prefix", prefix)];
        if let Some(t) = token {
            q.push(("continuation-token", t));
        }
        let r = self.call("GET", "", &q, &[], &[])?;
        if r.status != 200 {
            return Err(Self::error("LIST", &r));
        }
        let xml = String::from_utf8_lossy(&r.body).to_string();
        let mut out = Vec::new();
        for block in xml_blocks(&xml, "Contents") {
            if let Some(k) = xml_field(block, "Key") {
                let size = xml_field(block, "Size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                out.push((xml_unescape(&k), size));
            }
        }
        let next = if xml_field(&xml, "IsTruncated").as_deref() == Some("true") {
            xml_field(&xml, "NextContinuationToken").map(|s| xml_unescape(&s))
        } else {
            None
        };
        Ok((out, next))
    }

    pub fn create_multipart(&mut self, key: &str) -> Result<String, String> {
        let extra = vec![(
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        )];
        let r = self.call("POST", key, &[("uploads", "")], &extra, &[])?;
        if r.status != 200 {
            return Err(Self::error("CreateMultipartUpload", &r));
        }
        let xml = String::from_utf8_lossy(&r.body).to_string();
        xml_field(&xml, "UploadId")
            .map(|s| xml_unescape(&s))
            .ok_or_else(|| "storage returned no UploadId".into())
    }

    pub fn upload_part(
        &mut self,
        key: &str,
        upload: &str,
        n: u32,
        body: &[u8],
    ) -> Result<String, String> {
        let ns = n.to_string();
        let r = self.call(
            "PUT",
            key,
            &[("partNumber", &ns), ("uploadId", upload)],
            &[],
            body,
        )?;
        if r.status != 200 {
            return Err(Self::error("UploadPart", &r));
        }
        Self::etag(&r).ok_or_else(|| "storage returned no part ETag".into())
    }

    pub fn complete_multipart(
        &mut self,
        key: &str,
        upload: &str,
        parts: &[String],
    ) -> Result<(), String> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (i, e) in parts.iter().enumerate() {
            xml.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>\"{}\"</ETag></Part>",
                i + 1,
                e.trim_matches('"')
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let extra = vec![("content-type".to_string(), "application/xml".to_string())];
        let r = self.call("POST", key, &[("uploadId", upload)], &extra, xml.as_bytes())?;
        let text = String::from_utf8_lossy(&r.body);
        if r.status != 200 || text.contains("<Error>") {
            return Err(Self::error("CompleteMultipartUpload", &r));
        }
        Ok(())
    }

    pub fn abort_multipart(&mut self, key: &str, upload: &str) {
        let _ = self.call("DELETE", key, &[("uploadId", upload)], &[], &[]);
    }

    pub fn head_size(&mut self, key: &str) -> Result<Option<u64>, String> {
        let r = self.call("HEAD", key, &[], &[], &[])?;
        match r.status {
            200 => Ok(r.header("content-length").and_then(|v| v.parse().ok())),
            404 => Ok(None),
            s => Err(format!("storage HEAD failed: HTTP {s}")),
        }
    }
}

fn xml_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
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

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_encoding() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_735), (2026, 10, 9));
        assert_eq!(uri_encode("a b/c~", true), "a%20b/c~");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(
            S3::query(&[("uploads", ""), ("a", "b c")]),
            "a=b%20c&uploads="
        );
    }

    #[test]
    fn list_xml() {
        let x = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>a&amp;b</Key><Size>5</Size></Contents><NextContinuationToken>t1</NextContinuationToken></ListBucketResult>";
        let b = xml_blocks(x, "Contents");
        assert_eq!(xml_unescape(&xml_field(b[0], "Key").unwrap()), "a&b");
        assert_eq!(xml_field(x, "NextContinuationToken").as_deref(), Some("t1"));
    }
}
