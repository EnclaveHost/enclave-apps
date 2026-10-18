//! The HTTP side: a request for `<name>.eth` is served from whatever the
//! name points at, through the local software for that protocol.

use crate::ens::{Info, Lookup};
use crate::pages::{self, Missing};
use crate::State;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, HOST, LOCATION};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

pub type Body = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

pub fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|e| match e {}).boxed()
}

/// The protocols a name's contenthash can name, and the local software for each.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Ipfs,
    Swarm,
    Arweave,
    Tor,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Ipfs, Kind::Swarm, Kind::Arweave, Kind::Tor];
    pub fn label(self) -> &'static str {
        match self {
            Kind::Ipfs => "IPFS",
            Kind::Swarm => "Swarm",
            Kind::Arweave => "Arweave",
            Kind::Tor => "Tor",
        }
    }
}

/// Where a name's content lives.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// kubo's subdomain gateway host: `<cid>.ipfs.localhost` / `<key>.ipns.localhost`
    Ipfs { host: String, display: String },
    Swarm(String),
    Arweave(String),
    Onion(String),
    Unsupported(String),
}

pub fn target_of(contenthash: &str) -> Target {
    let ch = contenthash.trim();
    let ok_label = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if let Some(cid) = ch.strip_prefix("ipfs://").filter(|c| ok_label(c)) {
        return Target::Ipfs { host: format!("{}.ipfs.localhost", cid.to_ascii_lowercase()), display: ch.to_string() };
    }
    if let Some(key) = ch.strip_prefix("ipns://").filter(|c| ok_label(c)) {
        // kubo names IPNS keys in subdomains in base36 (`k51…`), and
        // redirects the base32 form ENS stores (`bafz…`) to it
        let key = base32_cid_to_base36(key).unwrap_or_else(|| key.to_ascii_lowercase());
        return Target::Ipfs { host: format!("{key}.ipns.localhost"), display: ch.to_string() };
    }
    if let Some(h) = ch.strip_prefix("bzz://").filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit())) {
        return Target::Swarm(h.to_ascii_lowercase());
    }
    if let Some(t) = ch.strip_prefix("ar://").filter(|t| ok_label(t)) {
        return Target::Arweave(t.to_string());
    }
    if let Some(o) = ch.strip_prefix("onion3://").filter(|o| o.len() == 56 && o.chars().all(|c| c.is_ascii_alphanumeric())) {
        return Target::Onion(o.to_ascii_lowercase());
    }
    Target::Unsupported(ch.to_string())
}

/// A multibase base32 CID (`b…`) rewritten as multibase base36 (`k…`).
fn base32_cid_to_base36(cid: &str) -> Option<String> {
    let body = cid.strip_prefix('b')?.to_ascii_lowercase();
    let (mut bits, mut acc, mut bytes) = (0u32, 0u32, Vec::new());
    for c in body.chars() {
        let v = "abcdefghijklmnopqrstuvwxyz234567".find(c)? as u32;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // base36 of the big-endian number, keeping leading zero bytes as '0'
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    let mut digits: Vec<u8> = Vec::new();
    let mut num = bytes[zeros..].to_vec();
    while !num.is_empty() {
        let (mut rem, mut next) = (0u32, Vec::with_capacity(num.len()));
        for b in &num {
            let cur = rem * 256 + *b as u32;
            let q = cur / 36;
            rem = cur % 36;
            if !(next.is_empty() && q == 0) {
                next.push(q as u8);
            }
        }
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[rem as usize]);
        num = next;
    }
    digits.extend(std::iter::repeat(b'0').take(zeros));
    digits.reverse();
    Some(format!("k{}", String::from_utf8(digits).ok()?))
}

/// Which local backends answer, checked at most every few seconds.
#[derive(Default)]
pub struct Backends {
    seen: Mutex<HashMap<Kind, (Instant, bool)>>,
    ipns_warm: Mutex<HashMap<String, Instant>>,
}

const RECHECK: Duration = Duration::from_secs(3);

impl Backends {
    pub async fn up(&self, st: &State, k: Kind) -> bool {
        if let Some((at, up)) = self.seen.lock().unwrap().get(&k) {
            if at.elapsed() < RECHECK {
                return *up;
            }
        }
        let up = probe(st, k).await;
        self.seen.lock().unwrap().insert(k, (Instant::now(), up));
        up
    }

    /// kubo's gateway resolves an IPNS name by waiting for many copies of
    /// its record (often its whole 30 s timeout); its API takes the first
    /// valid one. Resolving through the API first fills kubo's name cache,
    /// so the gateway then answers at once. Best effort: on failure the
    /// gateway resolves as it always does.
    pub async fn warm_ipns(&self, st: &State, key: &str) {
        if self.ipns_warm.lock().unwrap().get(key).is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
            return;
        }
        let url = format!("{}/api/v0/name/resolve?arg=/ipns/{key}&dht-record-count=1&dht-timeout=30s", st.cfg.ipfs_api);
        let ok = match reqwest::Client::builder().timeout(Duration::from_secs(35)).build() {
            Ok(c) => c.post(url).send().await.is_ok_and(|r| r.status().is_success()),
            Err(_) => false,
        };
        if ok {
            let mut m = self.ipns_warm.lock().unwrap();
            if m.len() > 1024 {
                m.clear();
            }
            m.insert(key.to_string(), Instant::now());
        }
    }

    /// A request just failed to connect: believe it over the cache.
    pub fn down(&self, k: Kind) {
        self.seen.lock().unwrap().insert(k, (Instant::now(), false));
    }
}

pub fn backend_url(st: &State, k: Kind) -> String {
    match k {
        Kind::Ipfs => st.cfg.ipfs.clone(),
        Kind::Swarm => st.cfg.bee.clone(),
        Kind::Arweave => st.cfg.arweave.clone(),
        Kind::Tor => format!("socks5://{}", st.cfg.tor),
    }
}

async fn probe(st: &State, k: Kind) -> bool {
    let t = Duration::from_millis(1500);
    let get = |url: String| async move {
        let c = reqwest::Client::builder().timeout(t).build().ok()?;
        c.get(url).send().await.ok().map(|r| r.status().is_success())
    };
    match k {
        // the empty identity CID: any kubo gateway answers it without the network
        Kind::Ipfs => get(format!("{}/ipfs/bafkqaaa", st.cfg.ipfs)).await == Some(true),
        Kind::Swarm => get(format!("{}/health", st.cfg.bee)).await == Some(true),
        Kind::Arweave => get(format!("{}/ar-io/info", st.cfg.arweave)).await == Some(true),
        Kind::Tor => tokio::time::timeout(t, tokio::net::TcpStream::connect(st.cfg.tor)).await.is_ok_and(|r| r.is_ok()),
    }
}

/// Serve HTTP, or HTTPS when given the acceptor.
pub async fn serve(l: TcpListener, st: Arc<State>, tls: Option<tokio_rustls::TlsAcceptor>) {
    loop {
        let Ok((c, _)) = l.accept().await else { continue };
        let (st, tls) = (st.clone(), tls.clone());
        tokio::spawn(async move {
            let https = tls.is_some();
            let svc = service_fn(move |req| {
                let st = st.clone();
                async move { Ok::<_, Infallible>(handle(&st, req, https).await) }
            });
            let b = hyper::server::conn::http1::Builder::new();
            match tls {
                None => {
                    let _ = b.serve_connection(TokioIo::new(c), svc).await;
                }
                Some(acc) => {
                    let Ok(Ok(t)) = tokio::time::timeout(Duration::from_secs(10), acc.accept(c)).await else { return };
                    let _ = b.serve_connection(TokioIo::new(t), svc).await;
                }
            }
        });
    }
}

fn host_of(req: &Request<Incoming>) -> String {
    let h = req.headers().get(HOST).and_then(|v| v.to_str().ok()).map(str::to_string).or_else(|| req.uri().host().map(str::to_string));
    let h = h.unwrap_or_default().to_ascii_lowercase();
    let h = h.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(h.as_str(), |(h, _)| h).to_string();
    h.trim_end_matches('.').to_string()
}

async fn handle(st: &State, req: Request<Incoming>, https: bool) -> Response<Body> {
    let host = host_of(&req);
    if req.uri().path() == "/.well-known/enclavedns-local" {
        return status_json(st).await;
    }
    let name = match host.strip_suffix(".ens") {
        Some(stem) => format!("{stem}.eth"),
        None => host.clone(),
    };
    if !name.ends_with(".eth") {
        return pages::home(st).await;
    }
    if !https && st.cfg.redirect_https {
        let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
        return pages::redirect(&format!("https://{host}{pq}"));
    }
    let info = match st.ens.info(&name).await {
        Lookup::Found(i) => i,
        Lookup::NoName => return pages::no_name(&name),
        Lookup::Unreachable(e) => return pages::unreachable(&name, st.ens.base(), &e),
    };
    if let Some(ch) = info.contenthash.clone() {
        return serve_content(st, req, &info, target_of(&ch)).await;
    }
    if let Some(url) = info.text.get("url").filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
        return pages::redirect(url);
    }
    pages::address_only(&info)
}

async fn serve_content(st: &State, req: Request<Incoming>, info: &Info, t: Target) -> Response<Body> {
    let (kind, display) = match &t {
        Target::Ipfs { display, .. } => (Kind::Ipfs, display.clone()),
        Target::Swarm(r) => (Kind::Swarm, format!("bzz://{r}")),
        Target::Arweave(id) => (Kind::Arweave, format!("ar://{id}")),
        Target::Onion(o) => (Kind::Tor, format!("{o}.onion")),
        Target::Unsupported(ch) => return pages::unsupported(info, ch),
    };
    let missing = Missing { name: &info.name, kind, content: &display, backend: backend_url(st, kind) };
    if !st.backends.up(st, kind).await {
        return pages::not_detected(&missing);
    }
    let r = match &t {
        Target::Ipfs { host: h, .. } => {
            if let Some(key) = h.strip_suffix(".ipns.localhost") {
                st.backends.warm_ipns(st, key).await;
            }
            proxy_http(&st.cfg.ipfs, req, String::new(), Some(h.clone())).await
        }
        Target::Swarm(r) => proxy_http(&st.cfg.bee, req, format!("/bzz/{r}"), None).await,
        Target::Arweave(id) => proxy_http(&st.cfg.arweave, req, format!("/{id}"), None).await,
        Target::Onion(o) => proxy_onion(st, req, o).await,
        Target::Unsupported(_) => unreachable!(),
    };
    match r {
        Ok(resp) => resp,
        Err(ProxyError::Connect) => {
            st.backends.down(kind);
            pages::not_detected(&missing)
        }
        Err(ProxyError::Other(e)) => pages::backend_failed(&missing, &e),
    }
}

enum ProxyError {
    /// the backend is not there (any more)
    Connect,
    Other(String),
}

const HOP_BY_HOP: [&str; 8] = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"];

fn strip_hop_by_hop(h: &mut hyper::HeaderMap) {
    for n in HOP_BY_HOP {
        h.remove(n);
    }
}

/// Forward to a local HTTP backend: `prefix` goes in front of the path (the
/// content's address there), or `vhost` names it as the Host (kubo's
/// subdomain gateway, which serves the content at the root so the site's
/// absolute links work).
async fn proxy_http(base: &str, req: Request<Incoming>, prefix: String, vhost: Option<String>) -> Result<Response<Body>, ProxyError> {
    let base_uri: Uri = base.parse().map_err(|e| ProxyError::Other(format!("bad backend URL {base}: {e}")))?;
    let authority = base_uri.authority().map(|a| a.to_string()).unwrap_or_default();
    let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let uri: Uri = format!("{}://{authority}{prefix}{pq}", base_uri.scheme_str().unwrap_or("http"))
        .parse()
        .map_err(|e| ProxyError::Other(format!("bad path: {e}")))?;
    let (mut parts, body) = req.into_parts();
    parts.uri = uri;
    strip_hop_by_hop(&mut parts.headers);
    let backend_host = match &vhost {
        Some(v) => format!("{v}:{}", base_uri.port_u16().unwrap_or(80)),
        None => authority.clone(),
    };
    parts.headers.insert(HOST, HeaderValue::from_str(&backend_host).map_err(|e| ProxyError::Other(e.to_string()))?);
    let client: Client<_, Incoming> = Client::builder(TokioExecutor::new()).build_http();
    let resp = client.request(Request::from_parts(parts, body)).await.map_err(|e| {
        if e.is_connect() {
            ProxyError::Connect
        } else {
            ProxyError::Other(e.to_string())
        }
    })?;
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    // a redirect to the backend's own address must stay on the .eth name
    // (and its scheme): make it relative to wherever the browser is
    if let Some(loc) = parts.headers.get(LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string) {
        let mut rest = None;
        for from in [format!("http://{backend_host}{prefix}"), format!("http://{backend_host}"), format!("http://{authority}{prefix}"), prefix.clone()] {
            if from.is_empty() {
                continue;
            }
            if let Some(r) = loc.strip_prefix(&from).filter(|r| r.is_empty() || r.starts_with('/') || r.starts_with('?')) {
                rest = Some(if r.is_empty() { "/".to_string() } else { r.to_string() });
                break;
            }
        }
        if let Some(v) = rest.and_then(|r| HeaderValue::from_str(&r).ok()) {
            parts.headers.insert(LOCATION, v);
        }
    }
    parts.headers.insert("x-enclavedns-local", HeaderValue::from_static("local backend"));
    Ok(Response::from_parts(parts, body.map_err(|e| Box::new(e) as _).boxed()))
}

/// Forward to `<onion>.onion:80` through the local Tor's SOCKS port.
async fn proxy_onion(st: &State, req: Request<Incoming>, onion: &str) -> Result<Response<Body>, ProxyError> {
    let target = format!("{onion}.onion");
    let stream = tokio::time::timeout(Duration::from_secs(60), tokio_socks::tcp::Socks5Stream::connect(st.cfg.tor, (target.as_str(), 80)))
        .await
        .map_err(|_| ProxyError::Other("Tor did not reach the onion service within 60 s".into()))?
        .map_err(|e| match e {
            tokio_socks::Error::Io(_) => ProxyError::Connect,
            e => ProxyError::Other(format!("Tor: {e}")),
        })?;
    let (mut send, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(|e| ProxyError::Other(e.to_string()))?;
    tokio::spawn(conn);
    let (mut parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    parts.uri = pq.parse().map_err(|e: hyper::http::uri::InvalidUri| ProxyError::Other(e.to_string()))?;
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.insert(HOST, HeaderValue::from_str(&target).map_err(|e| ProxyError::Other(e.to_string()))?);
    let resp = send.send_request(Request::from_parts(parts, body)).await.map_err(|e| ProxyError::Other(e.to_string()))?;
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.insert("x-enclavedns-local", HeaderValue::from_static("tor"));
    Ok(Response::from_parts(parts, body.map_err(|e| Box::new(e) as _).boxed()))
}

async fn status_json(st: &State) -> Response<Body> {
    let mut b = serde_json::Map::new();
    for k in Kind::ALL {
        b.insert(k.label().to_ascii_lowercase(), serde_json::json!({"at": backend_url(st, k), "detected": st.backends.up(st, k).await}));
    }
    let v = serde_json::json!({
        "service": "enclavedns-local",
        "version": env!("CARGO_PKG_VERSION"),
        "enclavedns": st.ens.base(),
        "address": st.cfg.gateway_ip.to_string(),
        "backends": b,
    });
    let mut r = Response::new(full(serde_json::to_vec_pretty(&v).unwrap_or_default()));
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
    *r.status_mut() = StatusCode::OK;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            target_of("ipns://bafzaajaiaejcbu2syj3gcpf6iexsnxps6rmshyuoaslwkhho2htaumo7ceybjnty"),
            Target::Ipfs {
                host: "k51qzi5uqu5dlg796airc94bhc0k0xhflyyrtp6k99z76rw5vknujh8lb21jeg.ipns.localhost".into(),
                display: "ipns://bafzaajaiaejcbu2syj3gcpf6iexsnxps6rmshyuoaslwkhho2htaumo7ceybjnty".into()
            }
        );
        assert!(matches!(target_of("ipfs://bafybeiabc"), Target::Ipfs { host, .. } if host == "bafybeiabc.ipfs.localhost"));
        assert_eq!(target_of("bzz://D1DE"), Target::Swarm("d1de".into()));
        assert_eq!(target_of("ar://abc-_DEF"), Target::Arweave("abc-_DEF".into()));
        let o = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid";
        assert_eq!(target_of(&format!("onion3://{o}")), Target::Onion(o.into()));
        assert_eq!(target_of("0xdeadbeef"), Target::Unsupported("0xdeadbeef".into()));
        // nothing that could smuggle a path or a host
        assert!(matches!(target_of("ipfs://a/b"), Target::Unsupported(_)));
        assert!(matches!(target_of("bzz://zz"), Target::Unsupported(_)));
        assert!(matches!(target_of("ar://a.b"), Target::Unsupported(_)));
    }
}
