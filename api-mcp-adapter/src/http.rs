//! Outbound HTTP over wasi:http/outgoing-handler - the app's ONE door to the
//! outside world, and the configured endpoints are the only things on the
//! other side of it. A port of eyesoff-ai's http.rs (the response cap, the
//! egress diagnosis), minus the heartbeat: this app holds no client stream
//! open while it waits, the MCP client does.
//!
//! The host owns the transport. It resolves the name, opens the socket, does
//! TLS, and synthesizes the `host` header from the authority (a guest may not
//! set `host` itself). On the fleet a deployment's outbound requests leave
//! through its egress front, which is IPv6-only: an endpoint with no AAAA
//! record cannot be dialled at all, and `egress_err` says so.

use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{
    Fields, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
};
use crate::bindings::wasi::io::streams::StreamError;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// the response was cut off at `max_bytes`
    pub truncated: bool,
}

pub struct HttpReq<'a> {
    pub method: Method,
    pub url: &'a str,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Option<&'a [u8]>,
    pub timeout_s: u64,
    pub max_bytes: usize,
}

impl<'a> HttpReq<'a> {
    pub fn get(url: &'a str) -> Self {
        Self {
            method: Method::Get,
            url,
            headers: Vec::new(),
            body: None,
            timeout_s: crate::engine::DEFAULT_TIMEOUT_S,
            max_bytes: crate::engine::DEFAULT_MAX_BYTES,
        }
    }

    pub fn header(mut self, name: &str, value: &[u8]) -> Self {
        self.headers.push((name.to_string(), value.to_vec()));
        self
    }
}

/// One outbound request, fully buffered both ways.
pub fn request(r: HttpReq) -> Result<Response, String> {
    let (scheme_s, authority, path) = split_url(r.url)?;
    let scheme = match scheme_s.as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => return Err(format!("unsupported scheme '{other}'")),
    };

    let fields = Fields::new();
    for (name, value) in &r.headers {
        let _ = fields.set(name, std::slice::from_ref(value));
    }
    if let Some(b) = r.body {
        // explicit content-length: without it wasi:http frames the body
        // chunked, which some server frontends reject outright
        let _ = fields.set(
            &"content-length".to_string(),
            &[b.len().to_string().into_bytes()],
        );
    }

    let req = OutgoingRequest::new(fields);
    let _ = req.set_method(&r.method);
    let _ = req.set_scheme(Some(&scheme));
    let _ = req.set_authority(Some(&authority));
    let _ = req.set_path_with_query(Some(&path));

    let opts = RequestOptions::new();
    // an image generation queued behind other tenants legitimately takes
    // minutes, so the ceiling is generous; the entry picks what fits it
    let ns = r.timeout_s.clamp(1, 600) * 1_000_000_000;
    let _ = opts.set_connect_timeout(Some(ns));
    let _ = opts.set_first_byte_timeout(Some(ns));

    let out_body = req.body().map_err(|_| "request body unavailable")?;
    let fut = outgoing_handler::handle(req, Some(opts))
        .map_err(|e| egress_err(&authority, &format!("{e}")))?;
    if let Some(b) = r.body {
        let stream = out_body.write().map_err(|_| "request stream unavailable")?;
        // the platform caps a single stream write at 4096 bytes
        for chunk in b.chunks(4000) {
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
        .map_err(|e| egress_err(&authority, &format!("{e}")))?;
    let status = resp.status();

    let mut out = Vec::new();
    let mut truncated = false;
    if let Ok(rbody) = resp.consume() {
        if let Ok(stream) = rbody.stream() {
            loop {
                match stream.blocking_read(64 * 1024) {
                    Ok(chunk) => {
                        out.extend_from_slice(&chunk);
                        if out.len() >= r.max_bytes {
                            out.truncate(r.max_bytes);
                            truncated = true;
                            break;
                        }
                    }
                    Err(StreamError::Closed) => break,
                    Err(_) => break,
                }
            }
        }
    }
    Ok(Response { status, body: out, truncated })
}

/// (scheme, authority, path-with-query). Authority keeps any port.
pub fn split_url(url: &str) -> Result<(String, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("not an absolute URL: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("URL has no host: {url}"));
    }
    Ok((scheme.to_ascii_lowercase(), authority, path))
}

/// Turn an outbound failure into an error that names the actual cause. On
/// this platform a refused connection almost always means one thing: the
/// deployment's egress is IPv6-only and the endpoint published no AAAA
/// record. An Enclave app URL gets its own answer, because the reflex there
/// is to go looking at the wrong deployment.
pub fn egress_err(authority: &str, err: &str) -> String {
    let host = authority.split(':').next().unwrap_or(authority);
    let is_literal = host == "localhost"
        || host.starts_with('[')
        || host.parse::<std::net::IpAddr>().is_ok();
    if !is_literal && (err.contains("ConnectionRefused") || err.contains("ConnectionTimeout")) {
        if host.ends_with(".app.enclave.host") {
            return format!(
                "cannot reach {host} ({err}). That is another Enclave deployment's app URL, and \
                 this deployment's outbound egress is IPv6-ONLY, so app-to-app depends on the \
                 gateway answering over v6 for that name (`dig AAAA {host}`); then check the \
                 target is up and funded."
            );
        }
        return format!(
            "cannot reach {host} ({err}). This deployment's outbound egress is IPv6-ONLY, \
             so a host with no AAAA record is unreachable - check with `dig AAAA {host}`."
        );
    }
    format!("request to {host} failed: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split() {
        let (s, a, p) = split_url("https://a.example:8443/x/y?q=1").unwrap();
        assert_eq!((s.as_str(), a.as_str(), p.as_str()), ("https", "a.example:8443", "/x/y?q=1"));
        assert_eq!(split_url("https://a.example").unwrap().2, "/");
        assert!(split_url("/relative").is_err());
    }

    #[test]
    fn refusals_explain_ipv6_only_egress() {
        let m = egress_err("api.example.com", "ErrorCode::ConnectionRefused");
        assert!(m.contains("IPv6-ONLY") && m.contains("dig AAAA api.example.com"), "{m}");
        let m = egress_err("da09d0f2.app.enclave.host", "ErrorCode::ConnectionRefused");
        assert!(m.contains("another Enclave deployment"), "{m}");
        for h in ["127.0.0.1", "localhost", "[::1]"] {
            assert!(!egress_err(h, "ErrorCode::ConnectionRefused").contains("IPv6-ONLY"), "{h}");
        }
        assert!(egress_err("x.y", "ErrorCode::TlsProtocolError").contains("TlsProtocolError"));
    }
}
