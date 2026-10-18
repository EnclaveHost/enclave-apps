//! The HTTP side: DNS-over-HTTPS (RFC 8484), a JSON resolve API in the
//! shape Google and Cloudflare use, the status page and its API.
//! TLS for all of it is terminated inside the enclave by the platform.

use crate::name::Name;
use crate::net::{self, Tcp};
use crate::rdata;
use crate::resolver::Sec;
use crate::server::{Server, Via};
use crate::wire::{rcode, ty, Message, Record};
use base64::Engine;
use serde_json::{json, Value};
use std::rc::Rc;
use std::time::Duration;

const UI: &str = include_str!("ui.html");

pub struct App {
    pub server: Rc<Server>,
    pub admin_token: Option<String>,
}

struct Req {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, n: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }
    fn param(&self, n: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }
}

struct Resp {
    status: u16,
    ctype: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

fn resp(status: u16, ctype: &'static str, body: impl Into<Vec<u8>>) -> Resp {
    Resp { status, ctype, headers: Vec::new(), body: body.into() }
}

fn json_resp(status: u16, v: Value) -> Resp {
    let mut r = resp(status, "application/json", v.to_string());
    r.headers.push(("access-control-allow-origin", "*".into()));
    r.headers.push(("cache-control", "no-store".into()));
    r
}

fn reason(s: u16) -> &'static str {
    match s {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub async fn serve_conn(app: Rc<App>, conn: Tcp) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let head_end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            if buf.len() > 16 * 1024 {
                let _ = write(&conn, resp(431, "text/plain", "headers too large"), true).await;
                return;
            }
            match net::timeout(Duration::from_secs(60), conn.read(16 * 1024)).await {
                Ok(d) if !d.is_empty() => buf.extend_from_slice(&d),
                _ => return,
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end - 4]).to_string();
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"), parts.next().unwrap_or("HTTP/1.0"));
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
        if len > 65536 {
            let _ = write(&conn, resp(413, "text/plain", "body too large"), true).await;
            return;
        }
        while buf.len() < head_end + len {
            match net::timeout(Duration::from_secs(30), conn.read(64 * 1024)).await {
                Ok(d) if !d.is_empty() => buf.extend_from_slice(&d),
                _ => return,
            }
        }
        let body = buf[head_end..head_end + len].to_vec();
        buf.drain(..head_end + len);
        let (path, qs) = target.split_once('?').unwrap_or((target, ""));
        let query = qs
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (pct_decode(k), pct_decode(v)),
                None => (pct_decode(p), String::new()),
            })
            .collect();
        let close = version == "HTTP/1.0"
            || headers.iter().any(|(k, v)| k == "connection" && v.eq_ignore_ascii_case("close"));
        let req = Req { method: method.to_string(), path: pct_decode(path), query, headers, body };
        let r = route(&app, &req).await;
        if write(&conn, r, close).await.is_err() || close {
            return;
        }
    }
}

async fn write(conn: &Tcp, r: Resp, close: bool) -> std::io::Result<()> {
    let mut h = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nx-content-type-options: nosniff\r\nreferrer-policy: no-referrer\r\n",
        r.status,
        reason(r.status),
        r.ctype,
        r.body.len()
    );
    for (k, v) in &r.headers {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    if close {
        h.push_str("connection: close\r\n");
    }
    h.push_str("\r\n");
    let mut out = h.into_bytes();
    out.extend_from_slice(&r.body);
    conn.write_all(&out).await
}

async fn route(app: &Rc<App>, req: &Req) -> Resp {
    if req.method == "OPTIONS" {
        let mut r = resp(204, "text/plain", "");
        r.headers.push(("access-control-allow-origin", "*".into()));
        r.headers.push(("access-control-allow-methods", "GET, POST, OPTIONS".into()));
        r.headers.push(("access-control-allow-headers", "content-type, accept".into()));
        r.headers.push(("access-control-max-age", "86400".into()));
        return r;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            let mut r = resp(200, "text/html; charset=utf-8", UI);
            r.headers.push((
                "content-security-policy",
                "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".into(),
            ));
            r.headers.push(("cache-control", "no-cache".into()));
            r
        }
        ("GET", "/healthz") | ("GET", "/ping") => resp(200, "text/plain", "ok\n"),
        (_, "/dns-query") => doh(app, req).await,
        ("GET", "/resolve") => json_resolve(app, req).await,
        ("GET", "/api/status") => json_resp(200, status(app)),
        ("GET", "/api/zones") => json_resp(200, zones(app)),
        ("POST", "/api/flush") => flush(app, req),
        ("GET", p) if p.starts_with("/api/ens/") => ens_info(app, &p["/api/ens/".len()..]).await,
        ("GET", p) if p.starts_with("/api/onion/") => onion_info(&p["/api/onion/".len()..]),
        (_, "/resolve") => json_resp(405, json!({"error": "method not allowed"})),
        _ => json_resp(404, json!({"error": "not found"})),
    }
}

/// RFC 8484: GET ?dns=<base64url>, or POST application/dns-message. A
/// `name=` parameter (or an application/dns-json Accept) is the JSON API.
async fn doh(app: &Rc<App>, req: &Req) -> Resp {
    let wire = match req.method.as_str() {
        "GET" => {
            if req.param("name").is_some() {
                return json_resolve(app, req).await;
            }
            let Some(d) = req.param("dns") else {
                return resp(400, "text/plain", "missing dns= parameter (RFC 8484) or name= (JSON)");
            };
            match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(d.trim_end_matches('=')) {
                Ok(w) => w,
                Err(_) => return resp(400, "text/plain", "dns= is not base64url"),
            }
        }
        "POST" => {
            let ct = req.header("content-type").unwrap_or("");
            if !ct.starts_with("application/dns-message") {
                return resp(415, "text/plain", "POST needs content-type application/dns-message");
            }
            req.body.clone()
        }
        _ => return resp(405, "text/plain", "GET or POST"),
    };
    let Some(answer) = app.server.handle(&wire, Via::Https).await else {
        return resp(400, "text/plain", "not a DNS query");
    };
    // cache for the smallest TTL in the answer (RFC 8484 §5.1)
    let ttl = Message::parse(&answer)
        .ok()
        .map(|m| m.answers.iter().chain(m.authority.iter()).map(|r| r.ttl).min().unwrap_or(0))
        .unwrap_or(0);
    let mut r = resp(200, "application/dns-message", answer);
    r.headers.push(("cache-control", format!("max-age={ttl}")));
    r.headers.push(("access-control-allow-origin", "*".into()));
    r
}

fn rec_json(r: &Record) -> Value {
    json!({
        "name": r.name.to_string(),
        "type": r.rtype,
        "typeName": ty::name(r.rtype),
        "TTL": r.ttl,
        "data": rdata::to_text(r.rtype, &r.data),
    })
}

async fn json_resolve(app: &Rc<App>, req: &Req) -> Resp {
    let Some(n) = req.param("name") else {
        return json_resp(400, json!({"error": "name= is required"}));
    };
    let name = match Name::parse(n) {
        Ok(n) => n,
        Err(e) => return json_resp(400, json!({"error": e})),
    };
    let t = req.param("type").unwrap_or("A");
    let Some(qtype) = ty::parse(t) else {
        return json_resp(400, json!({"error": format!("unknown type {t}")}));
    };
    if ty::is_meta(qtype) && qtype != ty::ANY {
        return json_resp(400, json!({"error": "that type cannot be queried"}));
    }
    let flag = |k: &str| req.param(k).is_some_and(|v| v == "1" || v == "true");
    let dnssec_ok = flag("do");
    let cd = flag("cd");
    let t0 = net::now_ms();
    let r = app.server.resolve(&name, qtype, dnssec_ok, cd).await;
    {
        let mut c = app.server.counters.borrow_mut();
        c.https += 1;
        c.answered += 1;
        c.latency_ms_total += net::now_ms() - t0;
    }
    let bogus = r.sec == Sec::Bogus && !cd;
    let status = if bogus { rcode::SERVFAIL } else { r.rcode };
    let keep = |x: &&Record| dnssec_ok || !matches!(x.rtype, ty::RRSIG | ty::NSEC | ty::NSEC3) || x.rtype == qtype;
    let mut v = json!({
        "Status": status,
        "StatusName": rcode::name(status),
        "TC": false,
        "RD": true,
        "RA": app.server.recursion,
        "AD": r.sec == Sec::Secure && !bogus,
        "CD": cd,
        "Question": [{"name": name.to_string(), "type": qtype, "typeName": ty::name(qtype)}],
        "Security": r.sec.name(),
        "Sources": r.sources,
        "ElapsedMs": net::now_ms() - t0,
    });
    if !bogus {
        v["Answer"] = Value::Array(r.answer.iter().filter(keep).map(rec_json).collect());
        v["Authority"] = Value::Array(r.authority.iter().filter(keep).map(rec_json).collect());
    }
    if let Some((c, w)) = &r.ede {
        v["EDE"] = json!({"code": c, "text": w});
        v["Comment"] = json!(w);
    }
    json_resp(200, v)
}

fn status(app: &Rc<App>) -> Value {
    let s = &app.server;
    let c = s.counters.borrow().clone();
    let st = s.resolver.stats.borrow().clone();
    let upstreams: Vec<Value> = s.resolver.cfg.forward.iter().zip(s.resolver.upstream_stats.borrow().iter()).map(|(u, x)| json!({
        "upstream": u.text(), "srtt_ms": x.srtt_ms, "answered": x.answered, "failed": x.failed,
    })).collect();
    let p = s.resolver.http.stats.borrow().clone();
    let outbound = json!({
        "h2_connects": p.h2_connects, "h2_requests": p.h2_requests, "h2_retries": p.h2_retries,
        "h1_connects": p.h1_connects, "h1_requests": p.h1_requests,
        "connect_ms_last": p.connect_ms_last, "connect_ms_max": p.connect_ms_max,
    });
    json!({
        "name": "EnclaveDNS",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": (net::now_ms() - s.started_ms) / 1000,
        "recursion": s.recursion,
        "startup": s.startup.borrow().iter().map(|(what, r)| match r {
            Ok(v) => json!({"what": what, "ok": true, "detail": v}),
            Err(e) => json!({"what": what, "ok": false, "error": e}),
        }).collect::<Vec<_>>(),
        "udp_replies": if s.udp_replies_blocked.get() { "blocked by the host's egress policy: use TCP/53, DoT or DoH" } else { "ok" },
        "resolver": {
            "mode": s.resolver.transport_text(),
            "validating": s.resolver.cfg.validate,
            "qname_minimisation": s.resolver.cfg.qname_minimisation,
            "cache_entries": s.resolver.cache_len(),
            "cache_limit": s.resolver.cfg.cache_entries,
            "upstream_queries": st.upstream_queries,
            "upstream_udp": st.upstream_udp,
            "upstream_tcp": st.upstream_tcp,
            "upstream_timeouts": st.upstream_timeouts,
            "cache_hits": st.cache_hits,
            "cache_misses": st.cache_misses,
            "stale_served": st.stale_served,
            "refreshes": st.refreshes,
            "prefetches": st.prefetches,
            "joined_in_flight": st.joined,
            "hedged": st.hedged,
            "upstreams": upstreams,
            "outbound": outbound,
            "validated": {"secure": st.secure, "insecure": st.insecure, "bogus": st.bogus},
            "trust_anchors": crate::dnssec::ROOT_ANCHORS.iter().map(|a| format!(". DS {} {} {} {}", a.0, a.1, a.2, a.3)).collect::<Vec<_>>(),
        },
        "queries": {
            "udp": c.udp, "tcp": c.tcp, "tls": c.tls, "https": c.https,
            "noerror": c.noerror, "nxdomain": c.nxdomain, "servfail": c.servfail, "refused": c.refused,
            "authenticated": c.ad, "rate_limited": c.rate_limited,
            "ens": c.ens, "onion": c.onion, "zone": c.zone,
            "mean_ms": if c.answered > 0 { c.latency_ms_total / c.answered } else { 0 },
        },
        "ens": s.ens.as_ref().map(|e| json!({
            "rpc": e.cfg.rpc.iter().map(|u| crate::tls::parse_url(u).map(|x| x.host).unwrap_or_default()).collect::<Vec<_>>(),
            "quorum": e.cfg.quorum,
            "gateway": e.cfg.gateway,
            "alias_ens_tld": s.ens_alias,
            "eth_calls": e.calls.get(),
        })),
        "onion": s.onion.as_ref().map(|o| json!({
            "gateway_a": o.a.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
            "gateway_aaaa": o.aaaa.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
            "cname_suffix": o.cname_suffix,
        })),
        "zones": s.zones.iter().map(|z| json!({
            "name": z.apex.to_string(),
            "records": z.record_count(),
            "signed": z.signer.is_some(),
            "ephemeral_key": z.ephemeral_key,
            "ds": z.ds_text(),
        })).collect::<Vec<_>>(),
        "logging": "none: this server keeps counters, never query names or client addresses",
    })
}

fn zones(app: &Rc<App>) -> Value {
    Value::Array(
        app.server
            .zones
            .iter()
            .map(|z| {
                json!({
                    "name": z.apex.to_string(),
                    "signed": z.signer.is_some(),
                    "ephemeral_key": z.ephemeral_key,
                    "ds": z.ds_text(),
                    "dnskey": z.dnskey_text(),
                    "records": z.listing(),
                })
            })
            .collect(),
    )
}

fn flush(app: &Rc<App>, req: &Req) -> Resp {
    let Some(tok) = &app.admin_token else {
        return json_resp(404, json!({"error": "no admin_token configured"}));
    };
    // the platform gateway strips Authorization; X-Api-Key survives
    let given = req.header("x-api-key").or_else(|| req.header("authorization").and_then(|v| v.strip_prefix("Bearer "))).unwrap_or("");
    if !ct_eq(given.as_bytes(), tok.as_bytes()) {
        return json_resp(401, json!({"error": "bad token"}));
    }
    let name = req.param("name").and_then(|n| Name::parse(n).ok());
    let n = app.server.resolver.flush(name.as_ref());
    json_resp(200, json!({"flushed": n}))
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn ens_info(app: &Rc<App>, raw: &str) -> Resp {
    let Some(ens) = &app.server.ens else {
        return json_resp(404, json!({"error": "ENS resolution is disabled"}));
    };
    let mut n = raw.trim_end_matches('.').to_ascii_lowercase();
    if let Some(stem) = n.strip_suffix(".ens") {
        n = format!("{stem}.eth");
    }
    if !n.ends_with(".eth") {
        return json_resp(400, json!({"error": "an ENS name ends in .eth"}));
    }
    let Ok(name) = Name::parse(&n) else {
        return json_resp(400, json!({"error": "bad name"}));
    };
    match ens.describe(&name).await {
        Ok(Some(i)) => json_resp(
            200,
            json!({
                "name": n,
                "resolver": i.resolver,
                "resolver_found_at": i.found_at,
                "wildcard": i.wildcard,
                "address": i.addr,
                "contenthash": i.contenthash,
                "dnslink": i.dnslink,
                "onion": i.onion,
                "text": i.texts.into_iter().map(|(k, v)| (k, Value::String(v))).collect::<serde_json::Map<String, Value>>(),
            }),
        ),
        Ok(None) => json_resp(404, json!({"name": n, "error": "no ENS resolver for this name"})),
        Err(e) => json_resp(502, json!({"name": n, "error": e})),
    }
}

fn onion_info(raw: &str) -> Resp {
    let n = raw.trim_end_matches('.').to_ascii_lowercase();
    let n = n.strip_suffix(".onion").unwrap_or(&n);
    let label = n.rsplit('.').next().unwrap_or("");
    match crate::onion::parse_v3(label) {
        Ok(s) => json_resp(
            200,
            json!({
                "address": format!("{}.onion", s.address),
                "valid": true,
                "version": 3,
                "ed25519_key": base64::engine::general_purpose::STANDARD.encode(s.key),
            }),
        ),
        Err(e) => json_resp(200, json!({"address": format!("{label}.onion"), "valid": false, "error": e})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent() {
        assert_eq!(pct_decode("a%2Eb+c"), "a.b c");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz"), "%zz");
    }
}
