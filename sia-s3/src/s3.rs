//! S3 request handling (path-style): the subset object stores are used for —
//! buckets are fixed by config; objects, ranges, copy, multipart, ListObjectsV2.

use std::rc::Rc;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::http::{Request, write_head};
use crate::sigv4::{self, Verdict, percent_decode};
use crate::store::Store;

pub struct App {
    pub store: std::cell::RefCell<Option<Rc<Store>>>,
    pub buckets: Vec<String>,
    pub credentials: Vec<(String, String)>,
    pub region: String,
    pub boot_error: std::cell::RefCell<Option<String>>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn xml(status: u16, body: String) -> Resp {
    Resp {
        status,
        headers: vec![("Content-Type".into(), "application/xml".into())],
        body: format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{body}").into_bytes(),
    }
}

fn err(status: u16, code: &str, msg: &str, resource: &str) -> Resp {
    xml(
        status,
        format!(
            "<Error><Code>{code}</Code><Message>{}</Message><Resource>{}</Resource></Error>",
            xml_escape(msg),
            xml_escape(resource)
        ),
    )
}

fn empty(status: u16) -> Resp {
    Resp { status, headers: vec![], body: vec![] }
}

fn http_date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0).unwrap_or_default().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn iso_date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

/// Map store errors onto S3 codes.
fn store_err(e: String, resource: &str) -> Resp {
    let code = e.split(':').next().unwrap_or("");
    match code {
        "NoSuchKey" => err(404, "NoSuchKey", "The specified key does not exist.", resource),
        "NoSuchUpload" => err(404, "NoSuchUpload", "The specified upload does not exist.", resource),
        "InvalidPart" | "InvalidPartOrder" => err(400, code, &e, resource),
        _ => {
            eprintln!("[sia-s3] {resource}: {e}");
            err(500, "InternalError", &e, resource)
        }
    }
}

/// Parse a single `bytes=` range against `size`: Ok(None) = whole object.
fn parse_range(h: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = h else { return Ok(None) };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None); // multi-range: serve the whole object, as S3 does
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        (size.saturating_sub(n), size.saturating_sub(1))
    } else {
        let s: u64 = a.parse().map_err(|_| ())?;
        let e: u64 = if b.is_empty() { size.saturating_sub(1) } else { b.parse::<u64>().map_err(|_| ())?.min(size.saturating_sub(1)) };
        (s, e)
    };
    if size == 0 || start >= size || end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

fn tag_values(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].trim().to_string());
        rest = &after[j + close.len()..];
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Handle one request, writing the full response to `w`.
pub async fn handle(app: &App, req: &Request, w: &mut TcpStream) -> std::io::Result<()> {
    let ka = req.keep_alive;
    if req.path == "/healthz" && req.method == "GET" {
        let body = health(app).await;
        let r = Resp { status: 200, headers: vec![("Content-Type".into(), "application/json".into())], body: body.into_bytes() };
        return send(w, r, ka).await;
    }
    let resource = req.path.clone();
    match sigv4::verify(req, &app.credentials) {
        Verdict::Ok => {}
        Verdict::Denied(code, msg) => return send(w, err(403, code, &msg, &resource), ka).await,
    }
    let Some(store) = app.store.borrow().clone() else {
        let msg = app.boot_error.borrow().clone().unwrap_or_else(|| "index is loading".into());
        return send(w, err(503, "SlowDown", &msg, &resource), ka).await;
    };

    let q = sigv4::parse_query(&req.query);
    let qv = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
    let has = |k: &str| q.iter().any(|(a, _)| a == k);

    let path = req.path.trim_start_matches('/');
    let (bucket_raw, key_raw) = path.split_once('/').unwrap_or((path, ""));
    let bucket = percent_decode(bucket_raw);
    let key = percent_decode(key_raw);

    if bucket.is_empty() {
        if req.method != "GET" {
            return send(w, err(405, "MethodNotAllowed", "", &resource), ka).await;
        }
        let bs: String = app
            .buckets
            .iter()
            .map(|b| format!("<Bucket><Name>{}</Name><CreationDate>2026-10-14T00:00:00.000Z</CreationDate></Bucket>", xml_escape(b)))
            .collect();
        return send(w, xml(200, format!("<ListAllMyBucketsResult><Owner><ID>sia-s3</ID></Owner><Buckets>{bs}</Buckets></ListAllMyBucketsResult>")), ka).await;
    }
    if !app.buckets.contains(&bucket) {
        return send(w, err(404, "NoSuchBucket", "The specified bucket does not exist.", &resource), ka).await;
    }

    if key.is_empty() {
        let r = match req.method.as_str() {
            "HEAD" => empty(200),
            "PUT" => empty(200), // buckets come from config; creating one that exists is a no-op
            "GET" if has("location") => xml(200, format!("<LocationConstraint>{}</LocationConstraint>", xml_escape(&app.region))),
            "GET" if has("versioning") => xml(200, "<VersioningConfiguration/>".into()),
            "GET" if has("uploads") => list_uploads(&store, &bucket).await,
            "GET" => list(&store, &bucket, &q).await,
            "POST" if has("delete") => delete_many(&store, &bucket, &req.body).await,
            _ => err(501, "NotImplemented", "bucket operation not supported", &resource),
        };
        return send(w, r, ka).await;
    }

    match req.method.as_str() {
        "GET" | "HEAD" if !has("uploadId") => get(&store, req, &bucket, &key, w).await,
        "PUT" if req.header("x-amz-copy-source").is_some() => {
            let src = percent_decode(req.header("x-amz-copy-source").unwrap().trim_start_matches('/'));
            let src = src.split('?').next().unwrap_or("").to_string();
            let (sb, sk) = src.split_once('/').unwrap_or((&src, ""));
            if !app.buckets.iter().any(|b| b == sb) || sk.is_empty() {
                return send(w, err(400, "InvalidArgument", "bad x-amz-copy-source", &resource), ka).await;
            }
            let r = match store.copy((sb, sk), (&bucket, &key)).await {
                Ok((etag, m)) => xml(200, format!("<CopyObjectResult><LastModified>{}</LastModified><ETag>\"{etag}\"</ETag></CopyObjectResult>", iso_date(m))),
                Err(e) => store_err(e, &resource),
            };
            send(w, r, ka).await
        }
        "PUT" if has("uploadId") => {
            let (Some(uid), Some(n)) = (qv("uploadId"), qv("partNumber").and_then(|n| n.parse::<u32>().ok())) else {
                return send(w, err(400, "InvalidArgument", "uploadId and partNumber are required", &resource), ka).await;
            };
            if !(1..=10000).contains(&n) {
                return send(w, err(400, "InvalidArgument", "partNumber must be 1-10000", &resource), ka).await;
            }
            let r = match store.upload_part(uid, n, req.body.clone()).await {
                Ok(etag) => Resp { status: 200, headers: vec![("ETag".into(), format!("\"{etag}\""))], body: vec![] },
                Err(e) => store_err(e, &resource),
            };
            send(w, r, ka).await
        }
        "PUT" => {
            let ct = req.header("content-type").map(String::from);
            let r = match store.put(&bucket, &key, req.body.clone(), ct).await {
                Ok(etag) => Resp { status: 200, headers: vec![("ETag".into(), format!("\"{etag}\""))], body: vec![] },
                Err(e) => store_err(e, &resource),
            };
            send(w, r, ka).await
        }
        "POST" if has("uploads") => {
            let uid = store.create_upload(&bucket, &key).await;
            let r = xml(200, format!(
                "<InitiateMultipartUploadResult><Bucket>{}</Bucket><Key>{}</Key><UploadId>{uid}</UploadId></InitiateMultipartUploadResult>",
                xml_escape(&bucket), xml_escape(&key)
            ));
            send(w, r, ka).await
        }
        "POST" if has("uploadId") => {
            let uid = qv("uploadId").unwrap_or("");
            let body = String::from_utf8_lossy(&req.body).to_string();
            let mut parts = Vec::new();
            for p in tag_values(&body, "Part") {
                let n = tag_values(&p, "PartNumber").first().and_then(|n| n.parse::<u32>().ok());
                let t = tag_values(&p, "ETag").first().map(|t| unescape(t).trim_matches('"').to_string());
                match (n, t) {
                    (Some(n), Some(t)) => parts.push((n, t)),
                    _ => return send(w, err(400, "MalformedXML", "bad CompleteMultipartUpload body", &resource), ka).await,
                }
            }
            let r = match store.complete_upload(uid, &parts).await {
                Ok((b, k, etag)) => xml(200, format!(
                    "<CompleteMultipartUploadResult><Location>/{b}/{k}</Location><Bucket>{}</Bucket><Key>{}</Key><ETag>\"{etag}\"</ETag></CompleteMultipartUploadResult>",
                    xml_escape(&b), xml_escape(&k)
                )),
                Err(e) => store_err(e, &resource),
            };
            send(w, r, ka).await
        }
        "DELETE" if has("uploadId") => {
            let r = match store.abort_upload(qv("uploadId").unwrap_or("")).await {
                Ok(()) => empty(204),
                Err(e) => store_err(e, &resource),
            };
            send(w, r, ka).await
        }
        "DELETE" => {
            let r = match store.delete(&bucket, &key).await {
                Ok(()) => empty(204),
                Err(e) => store_err(e, &resource),
            };
            send(w, r, ka).await
        }
        _ => send(w, err(501, "NotImplemented", "operation not supported", &resource), ka).await,
    }
}

async fn send(w: &mut TcpStream, r: Resp, ka: bool) -> std::io::Result<()> {
    write_head(w, r.status, &r.headers, r.body.len() as u64, ka).await?;
    if !r.body.is_empty() {
        w.write_all(&r.body).await?;
    }
    w.flush().await
}

async fn get(store: &Store, req: &Request, bucket: &str, key: &str, w: &mut TcpStream) -> std::io::Result<()> {
    let ka = req.keep_alive;
    let resource = req.path.clone();
    let (entry, segs, range) = {
        let idx = store.idx.lock().await;
        let Some(k) = idx.keys.get(&(bucket.to_string(), key.to_string())) else {
            let r = if req.method == "HEAD" { empty(404) } else { err(404, "NoSuchKey", "The specified key does not exist.", &resource) };
            return send(w, r, ka).await;
        };
        let size = k.entry.s;
        // conditional reads: a resumed range must not stitch two versions together
        let tag_matches = |h: &str| h.split(',').map(|t| t.trim().trim_start_matches("W/").trim_matches('"')).any(|t| t == "*" || t == k.entry.t);
        if req.header("if-match").is_some_and(|h| !tag_matches(h)) {
            return send(w, err(412, "PreconditionFailed", "At least one of the pre-conditions you specified did not hold", &resource), ka).await;
        }
        if req.header("if-none-match").is_some_and(tag_matches) {
            let r = Resp { status: 304, headers: vec![("ETag".into(), format!("\"{}\"", k.entry.t))], body: vec![] };
            return send(w, r, ka).await;
        }
        let range = match parse_range(req.header("range"), size) {
            Ok(r) => r,
            Err(()) => {
                let mut r = err(416, "InvalidRange", "The requested range is not satisfiable", &resource);
                r.headers.push(("Content-Range".into(), format!("bytes */{size}")));
                return send(w, r, ka).await;
            }
        };
        let (start, len) = match range {
            Some((a, b)) => (a, b - a + 1),
            None => (0, size),
        };
        let segs = if req.method == "HEAD" { Ok(vec![]) } else { idx.segments(k, start, len) };
        (k.entry.clone(), segs, range)
    };
    let segs = match segs {
        Ok(s) => s,
        Err(e) => return send(w, store_err(e, &resource), ka).await,
    };
    let mut headers = vec![
        ("ETag".to_string(), format!("\"{}\"", entry.t)),
        ("Last-Modified".to_string(), http_date(entry.m)),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        ("Content-Type".to_string(), entry.c.clone().unwrap_or_else(|| "application/octet-stream".into())),
    ];
    let (status, len) = match range {
        Some((a, b)) => {
            headers.push(("Content-Range".into(), format!("bytes {a}-{b}/{}", entry.s)));
            (206, b - a + 1)
        }
        None => (200, entry.s),
    };
    write_head(w, status, &headers, len, ka).await?;
    if req.method == "HEAD" {
        return w.flush().await;
    }
    let mut sent = 0u64;
    for (obj, off, n) in segs {
        match store.read_into(&obj, off, n, w).await {
            Ok(c) => sent += c,
            Err(e) => {
                // the head is already out: the only honest signal left is a short body
                eprintln!("[sia-s3] GET {resource}: download failed after {sent} bytes: {e}");
                return Err(std::io::Error::other(e));
            }
        }
    }
    if sent != len {
        return Err(std::io::Error::other(format!("short body {sent}/{len}")));
    }
    w.flush().await
}

async fn list(store: &Store, bucket: &str, q: &[(String, String)]) -> Resp {
    let qv = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let v2 = qv("list-type").as_deref() == Some("2");
    let prefix = qv("prefix").unwrap_or_default();
    let delimiter = qv("delimiter").filter(|d| !d.is_empty());
    let max = qv("max-keys").and_then(|m| m.parse::<usize>().ok()).unwrap_or(1000).min(1000);
    let after = if v2 {
        qv("continuation-token").map(|t| String::from_utf8(hex::decode(t).unwrap_or_default()).unwrap_or_default()).or(qv("start-after"))
    } else {
        qv("marker")
    }
    .unwrap_or_default();

    let idx = store.idx.lock().await;
    let lo = (bucket.to_string(), if after > prefix { after.clone() } else { prefix.clone() });
    let mut contents = String::new();
    let mut prefixes: Vec<String> = Vec::new();
    let mut count = 0;
    let mut last = String::new();
    let mut truncated = false;
    for ((b, k), key) in idx.keys.range(lo..) {
        if b != bucket || !k.starts_with(&prefix) {
            break;
        }
        if k == &after {
            continue;
        }
        if count == max {
            truncated = true;
            break;
        }
        if let Some(d) = &delimiter {
            if let Some(i) = k[prefix.len()..].find(d.as_str()) {
                let cp = k[..prefix.len() + i + d.len()].to_string();
                if prefixes.last() != Some(&cp) {
                    prefixes.push(cp);
                    count += 1;
                }
                last = k.clone();
                continue;
            }
        }
        contents.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
            xml_escape(k),
            iso_date(key.entry.m),
            key.entry.t,
            key.entry.s
        ));
        count += 1;
        last = k.clone();
    }
    let cps: String = prefixes.iter().map(|p| format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", xml_escape(p))).collect();
    let mut body = format!(
        "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>{}</Name><Prefix>{}</Prefix><MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>",
        xml_escape(bucket),
        xml_escape(&prefix)
    );
    if let Some(d) = &delimiter {
        body.push_str(&format!("<Delimiter>{}</Delimiter>", xml_escape(d)));
    }
    if v2 {
        body.push_str(&format!("<KeyCount>{count}</KeyCount>"));
        if let Some(t) = qv("continuation-token") {
            body.push_str(&format!("<ContinuationToken>{}</ContinuationToken>", xml_escape(&t)));
        }
        if truncated {
            body.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", hex::encode(&last)));
        }
    } else if truncated {
        body.push_str(&format!("<NextMarker>{}</NextMarker>", xml_escape(&last)));
    }
    body.push_str(&contents);
    body.push_str(&cps);
    body.push_str("</ListBucketResult>");
    xml(200, body)
}

async fn list_uploads(store: &Store, bucket: &str) -> Resp {
    let idx = store.idx.lock().await;
    let ups: String = idx
        .uploads
        .iter()
        .filter(|(_, u)| u.b == bucket)
        .map(|(id, u)| format!("<Upload><Key>{}</Key><UploadId>{id}</UploadId><Initiated>{}</Initiated></Upload>", xml_escape(&u.k), iso_date(u.started)))
        .collect();
    xml(200, format!("<ListMultipartUploadsResult><Bucket>{}</Bucket><IsTruncated>false</IsTruncated>{ups}</ListMultipartUploadsResult>", xml_escape(bucket)))
}

async fn delete_many(store: &Store, bucket: &str, body: &[u8]) -> Resp {
    let body = String::from_utf8_lossy(body).to_string();
    let quiet = tag_values(&body, "Quiet").first().is_some_and(|q| q == "true");
    let mut out = String::new();
    for o in tag_values(&body, "Object") {
        let Some(k) = tag_values(&o, "Key").first().map(|k| unescape(k)) else { continue };
        match store.delete(bucket, &k).await {
            Ok(()) if !quiet => out.push_str(&format!("<Deleted><Key>{}</Key></Deleted>", xml_escape(&k))),
            Ok(()) => {}
            Err(e) => out.push_str(&format!("<Error><Key>{}</Key><Code>InternalError</Code><Message>{}</Message></Error>", xml_escape(&k), xml_escape(&e))),
        }
    }
    xml(200, format!("<DeleteResult>{out}</DeleteResult>"))
}

async fn health(app: &App) -> String {
    let store = app.store.borrow().clone();
    match store {
        None => serde_json::json!({ "app": concat!("sia-s3/", env!("CARGO_PKG_VERSION")), "ready": false, "error": *app.boot_error.borrow() }).to_string(),
        Some(s) => {
            let idx = s.idx.lock().await;
            let unpacked = idx.objs.values().filter(|(o, m)| !m.p && o.size() <= s.compact_max_object).count();
            serde_json::json!({
                "app": concat!("sia-s3/", env!("CARGO_PKG_VERSION")),
                "ready": true,
                "keys": idx.keys.len(),
                "objects": idx.objs.len(),
                "openUploads": idx.uploads.len(),
                "smallUnpacked": unpacked,
                "bytes": idx.keys.values().map(|k| k.entry.s).sum::<u64>(),
                "compactor": *s.stats.borrow(),
            })
            .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(Some("bytes=0-99"), 1000), Ok(Some((0, 99))));
        assert_eq!(parse_range(Some("bytes=900-"), 1000), Ok(Some((900, 999))));
        assert_eq!(parse_range(Some("bytes=-100"), 1000), Ok(Some((900, 999))));
        assert_eq!(parse_range(Some("bytes=990-2000"), 1000), Ok(Some((990, 999))));
        assert!(parse_range(Some("bytes=1000-"), 1000).is_err());
        assert_eq!(parse_range(None, 10), Ok(None));
    }
}
