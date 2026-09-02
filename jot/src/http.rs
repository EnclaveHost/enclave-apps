//! Outbound HTTP over wasi:http/outgoing-handler: the app's ONE door to the
//! outside world, and the bucket is the only thing on the other side of it.
//!
//! The host owns the transport. It resolves the name, opens the socket, does
//! TLS, and synthesizes the `host` header from the authority (a guest may not
//! set `host` itself: it is a forbidden header by contract). That last fact
//! matters to the S3 client, which signs `host` and must sign exactly what the
//! host will send, so the authority handed in here IS the signed host value.
//!
//! On the fleet, a deployment's outbound requests leave through its egress
//! front, which is IPv6-only: a bucket endpoint with no AAAA record cannot be
//! dialled at all. R2 (`*.r2.cloudflarestorage.com`) and AWS S3 are dual-stack;
//! `egress_err` says so in the failure instead of leaving an operator hunting
//! for a bad key.

use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{
    Fields, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
};
use crate::bindings::wasi::io::streams::StreamError;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// every response header, lowercased
    pub headers: Vec<(String, String)>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct HttpReq<'a> {
    pub method: Method,
    pub https: bool,
    /// host[:port], exactly as the host will put it in the `host` header
    pub authority: &'a str,
    pub path_with_query: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub timeout_s: u64,
    pub max_bytes: usize,
}

/// One outbound request, fully buffered both ways (notes are small by
/// construction: the write path caps a note at 1 MiB and the read path caps
/// the response at `max_bytes`).
pub fn request(r: &HttpReq) -> Result<Response, String> {
    let fields = Fields::new();
    for (name, value) in r.headers {
        fields
            .set(name, &[value.as_bytes().to_vec()])
            .map_err(|e| format!("header {name}: {e:?}"))?;
    }
    let has_body_method = matches!(r.method, Method::Put | Method::Post | Method::Patch);
    if has_body_method || !r.body.is_empty() {
        // explicit content-length: without it wasi:http frames the body
        // chunked, and S3 wants the length up front on a PUT
        let _ = fields.set(
            &"content-length".to_string(),
            &[r.body.len().to_string().into_bytes()],
        );
    }

    let req = OutgoingRequest::new(fields);
    let _ = req.set_method(&r.method);
    let scheme = if r.https { Scheme::Https } else { Scheme::Http };
    let _ = req.set_scheme(Some(&scheme));
    let _ = req.set_authority(Some(r.authority));
    let _ = req.set_path_with_query(Some(r.path_with_query));

    let opts = RequestOptions::new();
    let ns = r.timeout_s.clamp(1, 120) * 1_000_000_000;
    let _ = opts.set_connect_timeout(Some(ns));
    let _ = opts.set_first_byte_timeout(Some(ns));
    let _ = opts.set_between_bytes_timeout(Some(ns));

    let out_body = req.body().map_err(|_| "request body unavailable")?;
    let fut = outgoing_handler::handle(req, Some(opts))
        .map_err(|e| egress_err(r.authority, &format!("{e}")))?;
    if !r.body.is_empty() {
        let stream = out_body.write().map_err(|_| "request stream unavailable")?;
        // the platform caps a single stream write at 4096 bytes
        for chunk in r.body.chunks(4000) {
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|e| format!("send body: {e}"))?;
        }
        drop(stream);
    }
    OutgoingBody::finish(out_body, None).map_err(|e| format!("finish body: {e}"))?;

    fut.subscribe().block();
    let resp = fut
        .get()
        .ok_or("no response")?
        .map_err(|_| "response taken twice")?
        .map_err(|e| egress_err(r.authority, &format!("{e}")))?;
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers()
        .entries()
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), String::from_utf8_lossy(&v).into_owned()))
        .collect();

    let mut out = Vec::new();
    if let Ok(rbody) = resp.consume() {
        if let Ok(stream) = rbody.stream() {
            loop {
                match stream.blocking_read(64 * 1024) {
                    Ok(chunk) => {
                        out.extend_from_slice(&chunk);
                        if out.len() > r.max_bytes {
                            return Err(format!(
                                "response from {} exceeds {} bytes",
                                r.authority, r.max_bytes
                            ));
                        }
                    }
                    Err(StreamError::Closed) => break,
                    Err(_) => break,
                }
            }
        }
    }
    Ok(Response { status, body: out, headers })
}

/// Turn an outbound failure into an error that names the likely cause. On
/// this platform a refused connection almost always means one thing: the
/// deployment's egress is IPv6-only and the endpoint published no AAAA record.
pub fn egress_err(authority: &str, err: &str) -> String {
    let host = authority.split(':').next().unwrap_or(authority);
    let is_literal = host == "localhost"
        || host.starts_with('[')
        || host.parse::<std::net::IpAddr>().is_ok();
    if !is_literal && (err.contains("ConnectionRefused") || err.contains("ConnectionTimeout")) {
        return format!(
            "cannot reach {host} ({err}). This deployment's outbound egress is IPv6-only, \
             so an S3 endpoint with no AAAA record is unreachable: check with `dig AAAA {host}`. \
             R2 (*.r2.cloudflarestorage.com) and AWS S3 are dual-stack and work."
        );
    }
    format!("request to {host} failed: {err}")
}
