//! Outbound HTTP over wasi:http/outgoing-handler - the app's ONE door to the
//! outside world. Both the web-search leg (search.rs) and the image-generation
//! leg (image.rs) go through here, so the timeout, the response cap and the
//! egress diagnosis are stated once instead of drifting between copies.

use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{
    Fields, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
};
use crate::bindings::wasi::io::streams::StreamError;

/// A sane default cap for JSON and HTML. Callers that expect something large
/// (a PNG comes back base64'd, so ~1.4x its already-megabyte size) must raise
/// it explicitly - silently truncating a response is how you get a corrupt
/// image and a baffling parse error instead of an honest "too big".
pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub location: Option<String>,
    pub ctype: Option<String>,
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
            timeout_s: 15,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn post(url: &'a str, body: &'a [u8]) -> Self {
        Self {
            method: Method::Post,
            url,
            headers: Vec::new(),
            body: Some(body),
            timeout_s: 15,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    pub fn header(mut self, name: &str, value: &[u8]) -> Self {
        self.headers.push((name.to_string(), value.to_vec()));
        self
    }

    pub fn timeout(mut self, s: u64) -> Self {
        self.timeout_s = s;
        self
    }

    pub fn max_bytes(mut self, n: usize) -> Self {
        self.max_bytes = n;
        self
    }
}

/// One outbound request.
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
    // image generation legitimately takes minutes on a busy share, so the
    // ceiling here is generous; the CALLER picks what is reasonable for it
    let ns = r.timeout_s.clamp(1, 600) * 1_000_000_000;
    let _ = opts.set_connect_timeout(Some(ns));
    let _ = opts.set_first_byte_timeout(Some(ns));

    let out_body = req.body().map_err(|_| "request body unavailable")?;
    let fut = outgoing_handler::handle(req, Some(opts))
        .map_err(|e| egress_err(&authority, &format!("{e}")))?;
    if let Some(b) = r.body {
        let stream = out_body.write().map_err(|_| "request stream unavailable")?;
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
    let rh = resp.headers();
    let location = first_header(&rh, "location");
    let ctype = first_header(&rh, "content-type");

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
    Ok(Response { status, body: out, location, ctype, truncated })
}

pub fn first_header(fields: &Fields, name: &str) -> Option<String> {
    fields
        .get(&name.to_string())
        .into_iter()
        .next()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
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

/// Resolve a Location value against the URL it came from: absolute URLs win,
/// `/path` keeps the origin, anything else is relative to the current dir.
pub fn resolve_url(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    if let Some(rest) = loc.strip_prefix("//") {
        let scheme = if base.starts_with("http://") { "http" } else { "https" };
        return format!("{scheme}://{rest}");
    }
    let (scheme, authority, path) = match split_url(base) {
        Ok(p) => p,
        Err(_) => return loc.to_string(),
    };
    if loc.starts_with('/') {
        return format!("{scheme}://{authority}{loc}");
    }
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    format!("{scheme}://{authority}{dir}{loc}")
}

/// Turn an outbound failure into an error that names the actual cause.
///
/// A bare "ErrorCode::ConnectionRefused" sends you hunting for a bad API key,
/// a firewall or a TLS problem. On this platform it almost always means one
/// thing: a deployment's outbound egress leaves from its own dedicated IPv6
/// and is IPv6-ONLY, so a host that publishes no AAAA record cannot be
/// dialled. Measured against a live deployment 2026-07-29 - example.com,
/// wikipedia, serpapi and exa (all dual-stack) connected; brave, serper and
/// duckduckgo (all IPv4-only) were refused, every time.
pub fn egress_err(authority: &str, err: &str) -> String {
    let host = authority.split(':').next().unwrap_or(authority);
    // a literal address or loopback is local dev, not the IPv6 story - telling
    // someone to `dig AAAA 127.0.0.1` is just noise
    let is_literal = host == "localhost"
        || host.starts_with('[')
        || host.parse::<std::net::IpAddr>().is_ok();
    if !is_literal && (err.contains("ConnectionRefused") || err.contains("ConnectionTimeout")) {
        // An Enclave app URL deserves its own answer. The generic "pick a
        // dual-stack provider" advice is useless here - you cannot pick a
        // different DNS record for your own deployment, and the reflex is to
        // go looking at the target app, which is fine. Measured 2026-07-29:
        // *.app.enclave.host resolves A-only (46.62.128.36, no AAAA) while
        // the apex enclave.host is dual-stack, so app-to-app HTTP over the
        // public URL cannot connect in either direction.
        if host.ends_with(".app.enclave.host") {
            return format!(
                "cannot reach {host} ({err}). That is another Enclave deployment's app URL, \
                 and *.app.enclave.host publishes an A record only - while this deployment's \
                 outbound egress is IPv6-ONLY. So one Enclave app cannot call another over \
                 its public URL, whatever either app does: the gateway needs an AAAA. \
                 Nothing to fix in the target app - confirm with `dig AAAA {host}`."
            );
        }
        return format!(
            "cannot reach {host} ({err}). This deployment's outbound egress is IPv6-ONLY, \
             so a host with no AAAA record is unreachable - check with `dig AAAA {host}`. \
             brave, serper, tavily and duckduckgo are IPv4-only and CANNOT be used here; \
             exa and serpapi are dual-stack and work."
        );
    }
    format!("request to {host} failed: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_and_resolve() {
        let (s, a, p) = split_url("https://a.example:8443/x/y?q=1").unwrap();
        assert_eq!((s.as_str(), a.as_str(), p.as_str()), ("https", "a.example:8443", "/x/y?q=1"));
        assert_eq!(split_url("https://a.example").unwrap().2, "/");
        assert!(split_url("/relative").is_err());

        let base = "https://a.example/dir/page.html";
        assert_eq!(resolve_url(base, "https://b.example/z"), "https://b.example/z");
        assert_eq!(resolve_url(base, "//b.example/z"), "https://b.example/z");
        assert_eq!(resolve_url(base, "/root"), "https://a.example/root");
        assert_eq!(resolve_url(base, "sibling.html"), "https://a.example/dir/sibling.html");
    }

    #[test]
    fn connection_refused_explains_ipv6_only_egress() {
        // the message that would have saved a debugging session
        let m = egress_err("api.search.brave.com", "ErrorCode::ConnectionRefused");
        assert!(m.contains("IPv6-ONLY"), "{m}");
        assert!(m.contains("dig AAAA api.search.brave.com"), "{m}");
        // ports are stripped from the advice
        assert!(!egress_err("api.exa.ai:443", "ErrorCode::ConnectionRefused").contains(":443"));
        // literal addresses are local dev, not the IPv6 story
        for h in ["127.0.0.1", "localhost", "[::1]"] {
            let m = egress_err(h, "ErrorCode::ConnectionRefused");
            assert!(!m.contains("IPv6-ONLY"), "{h}: {m}");
        }
        // unrelated failures keep their own wording
        let m = egress_err("api.exa.ai", "ErrorCode::TlsProtocolError");
        assert!(!m.contains("IPv6-ONLY"), "{m}");
        assert!(m.contains("TlsProtocolError"), "{m}");
    }

    #[test]
    fn app_to_app_gets_its_own_diagnosis() {
        let m = egress_err("da09d0f2.app.enclave.host", "ErrorCode::ConnectionRefused");
        assert!(m.contains("another Enclave deployment"), "{m}");
        assert!(m.contains("Nothing to fix in the target app"), "{m}");
        // the generic provider advice would be actively misleading here
        assert!(!m.contains("exa and serpapi"), "{m}");
        // a non-Enclave host still gets the provider advice
        let m = egress_err("api.search.brave.com", "ErrorCode::ConnectionRefused");
        assert!(m.contains("exa and serpapi"), "{m}");
        assert!(!m.contains("another Enclave deployment"), "{m}");
    }
}
