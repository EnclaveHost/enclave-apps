//! Outbound HTTP over wasi:http/outgoing-handler - the app's ONE door to the
//! outside world. Both the web-search leg (search.rs) and the image-generation
//! leg (image.rs) go through here, so the timeout, the response cap and the
//! egress diagnosis are stated once instead of drifting between copies.

use crate::bindings::wasi::clocks::monotonic_clock;
use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{
    Fields, Method, OutgoingBody, OutgoingRequest, RequestOptions, Scheme,
};
use crate::bindings::wasi::io::poll;
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
    /// every response header, lowercased. `location` and `ctype` above are the
    /// two this app has always needed by name; MCP needs one more
    /// (`mcp-session-id`) and a future caller will need another, so the whole
    /// set is kept rather than growing a field per protocol.
    pub headers: Vec<(String, String)>,
    /// the response was cut off at `max_bytes`
    pub truncated: bool,
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
    request_with_tick(r, 0, &mut |_| true)
}

/// `request`, but with a heartbeat: while waiting for the response's FIRST
/// byte, `tick(total_seconds_waited)` fires every `tick_s` seconds (0 = never
/// tick, identical to `request`); a tick that returns false abandons the
/// request - the reader left, nobody will read the answer - and the call
/// returns "client disconnected". The first-byte wait is where a slow leg
/// spends its whole life - an image generation queued behind other tenants
/// answers with one JSON blob only when it is DONE - and callers that hold a
/// client-facing stream open during that wait need something to write into
/// it, or every idle-timeout between here and the browser is entitled to
/// conclude the connection is dead. The tick does not extend the deadline:
/// `timeout_s` still bounds the wait host-side via first_byte_timeout.
pub fn request_with_tick(
    r: HttpReq,
    tick_s: u64,
    tick: &mut dyn FnMut(u64) -> bool,
) -> Result<Response, String> {
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
    // and the BODY: without this a stalled stream sits at the host default
    // (minutes), long past the timeout the caller asked for. It bounds one
    // gap between chunks; the deadline in the drain loop below bounds the
    // whole read, which is what a keepalive-emitting stream needs.
    let _ = opts.set_between_bytes_timeout(Some(ns));

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

    let ready = fut.subscribe();
    if tick_s == 0 {
        ready.block();
    } else {
        let mut waited_s = 0u64;
        while !ready.ready() {
            let timer = monotonic_clock::subscribe_duration(tick_s * 1_000_000_000);
            let woke = poll::poll(&[&ready, &timer]);
            if woke.contains(&0) || ready.ready() {
                break;
            }
            waited_s += tick_s;
            if !tick(waited_s) {
                // dropping the future abandons the request host-side
                return Err("client disconnected".into());
            }
        }
    }
    let resp = fut
        .get()
        .ok_or("no response")?
        .map_err(|_| "response taken twice")?
        .map_err(|e| egress_err(&authority, &format!("{e}")))?;
    let status = resp.status();
    let rh = resp.headers();
    let location = first_header(&rh, "location");
    let ctype = first_header(&rh, "content-type");
    let headers: Vec<(String, String)> = rh
        .entries()
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), String::from_utf8_lossy(&v).into_owned()))
        .collect();

    let mut out = Vec::new();
    let mut truncated = false;
    // the whole read, not just one gap in it: an SSE response is drained
    // until the server closes the stream, and the transport spec only SAYS
    // a server SHOULD close after answering. A stream held open with
    // keepalives resets the between-bytes timer forever, so the total is
    // bounded here and the failure says so instead of reporting a phantom
    // "cut off at N bytes".
    let deadline = monotonic_clock::now().saturating_add(ns);
    let mut timed_out = false;
    if let Ok(rbody) = resp.consume() {
        if let Ok(stream) = rbody.stream() {
            loop {
                if monotonic_clock::now() >= deadline {
                    timed_out = true;
                    break;
                }
                match stream.blocking_read(64 * 1024) {
                    Ok(chunk) => {
                        out.extend_from_slice(&chunk);
                        // one byte PAST the cap before calling it truncated:
                        // a response of exactly max_bytes is whole
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
    if timed_out && out.is_empty() {
        return Err(format!(
            "the response body from {authority} did not finish within {}s",
            r.timeout_s
        ));
    }
    Ok(Response { status, body: out, location, ctype, headers, truncated })
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
                 a missing AAAA record alone does not establish an egress failure. This deployment can test the path with an authenticated GET /search?url=https://{host}/ping request."
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
