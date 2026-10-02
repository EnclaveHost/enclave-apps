//! Outbound HTTP over wasi:http/outgoing-handler - the app's ONE door to the
//! outside world, and the configured endpoints are the only things on the
//! other side of it. A port of eyesoff-ai's http.rs (the response cap, the
//! egress diagnosis), minus the heartbeat: this app holds no client stream
//! open while it waits, the MCP client does.
//!
//! The host owns the transport. It resolves the name, opens the socket, does
//! TLS, and synthesizes the `host` header from the authority (a guest may not
//! set `host` itself). On the fleet a deployment's outbound requests leave
//! through its configured outbound route. Connection failures need live DNS,
//! route and destination checks; they do not establish an IP-family limit.

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
        // NOT ignored: the host refuses forbidden and malformed headers, and
        // dropping one silently sends the request without the credential it
        // was supposed to carry - which arrives as an unexplainable 401
        fields
            .set(name, std::slice::from_ref(value))
            .map_err(|e| format!("header '{name}' was refused by the host: {e:?}"))?;
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
                        // read ONE byte past the cap before calling it
                        // truncated: a response of exactly max_bytes is
                        // whole, and reporting it as cut off would make
                        // every exactly-sized answer look damaged
                        if out.len() > r.max_bytes {
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

/// Preserve the transport error without inferring DNS, IP-family, or funding
/// state from a connection refusal. These checks require live evidence.
pub fn egress_err(authority: &str, err: &str) -> String {
    let host = if authority.starts_with('[') {
        authority.split(']').next().unwrap_or(authority).trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    if err.contains("ConnectionRefused") || err.contains("ConnectionTimeout") {
        if host.ends_with(".app.enclave.host") {
            return format!(
                "cannot reach {host} ({err}). Check that this Enclave deployment has an active \
                 host lease and a current public route. Check both A and AAAA DNS records; \
                 a missing AAAA record alone does not establish an egress failure."
            );
        }
        return format!(
            "cannot reach {host} ({err}). Check the target's DNS records, listening port and \
             this deployment's outbound route. This error alone does not identify the cause."
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
    fn connection_errors_preserve_evidence_without_guessing_ip_family() {
        for cause in ["ErrorCode::ConnectionRefused", "ErrorCode::ConnectionTimeout"] {
            for host in ["api.example.com", "127.0.0.1", "localhost", "[2001:db8::1]:443"] {
                let m = egress_err(host, cause);
                assert!(m.contains(cause), "{m}");
                assert!(!m.contains("IPv6-ONLY"), "{m}");
            }
            let m = egress_err("38f368d6.app.enclave.host:443", cause);
            assert!(m.contains("active host lease") && m.contains("public route"), "{m}");
            assert!(!m.contains(":443"), "{m}");
        }
        assert!(egress_err("[2001:db8::1]:443", "TlsProtocolError").contains("2001:db8::1"));
        assert_eq!(egress_err("api.example.com", "TlsProtocolError"), "request to api.example.com failed: TlsProtocolError");
    }
}
