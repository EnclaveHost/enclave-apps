//! The slice of the reqwest API that `sia_storage` uses, for wasm32-wasip2.
//!
//! reqwest picks its browser (fetch/wasm-bindgen) backend on every wasm32
//! target, which cannot run outside a browser. This crate is aliased in as
//! `reqwest` on `target_os = "wasi"` and speaks plain HTTP/1.1 over tokio +
//! rustls instead. Every request is `Connection: close` — the indexer API is
//! a handful of small signed calls, not a hot path.
//!
//! [`net::connect`] is the one dialer: when `ENCLAVE_EGRESS`
//! (`socks5://<id>:<token>@host:port`) is set, as it is for an enclave
//! deployment with egress, every connection goes through that SOCKS5 front;
//! otherwise it is a direct connect (local `wasmtime -S inherit-network`).

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub use http::{Method, StatusCode};
pub use url::Url;

pub mod header {
    pub use http::header::{
        ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
        InvalidHeaderValue,
    };
}

use header::{HeaderMap, HeaderName, HeaderValue};

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "http: {}", self.0)
    }
}

impl std::error::Error for Error {}

impl Error {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
    pub fn is_timeout(&self) -> bool {
        self.0.starts_with("timeout")
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(format!("i/o: {e}"))
    }
}

impl From<url::ParseError> for Error {
    fn from(e: url::ParseError) -> Self {
        Self(format!("url: {e}"))
    }
}

pub trait IntoUrl {
    fn into_url(self) -> Result<Url, Error>;
}

impl IntoUrl for Url {
    fn into_url(self) -> Result<Url, Error> {
        Ok(self)
    }
}

impl IntoUrl for &Url {
    fn into_url(self) -> Result<Url, Error> {
        Ok(self.clone())
    }
}

impl IntoUrl for &str {
    fn into_url(self) -> Result<Url, Error> {
        Ok(Url::parse(self)?)
    }
}

impl IntoUrl for String {
    fn into_url(self) -> Result<Url, Error> {
        Ok(Url::parse(&self)?)
    }
}

impl IntoUrl for &String {
    fn into_url(self) -> Result<Url, Error> {
        Ok(Url::parse(self)?)
    }
}

#[derive(Clone, Default)]
pub struct Client {
    default_headers: HeaderMap,
}

#[derive(Default)]
pub struct ClientBuilder {
    default_headers: HeaderMap,
}

impl ClientBuilder {
    pub fn default_headers(mut self, headers: HeaderMap) -> Self {
        self.default_headers = headers;
        self
    }

    pub fn build(self) -> Result<Client, Error> {
        Ok(Client { default_headers: self.default_headers })
    }
}

impl Client {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        RequestBuilder {
            method,
            url: url.into_url(),
            headers: self.default_headers.clone(),
            body: None,
            timeout: None,
        }
    }

    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }
}

pub struct RequestBuilder {
    method: Method,
    url: Result<Url, Error>,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
}

impl RequestBuilder {
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Appends the serialized pairs to the URL's query, as reqwest does.
    pub fn query<T: Serialize + ?Sized>(mut self, q: &T) -> Self {
        if let Ok(url) = &mut self.url {
            match serde_urlencoded::to_string(q) {
                Ok(s) if s.is_empty() => {}
                Ok(s) => {
                    let joined = match url.query() {
                        Some(old) if !old.is_empty() => format!("{old}&{s}"),
                        _ => s,
                    };
                    url.set_query(Some(&joined));
                }
                Err(e) => self.url = Err(Error::new(format!("query: {e}"))),
            }
        }
        self
    }

    pub fn header<K, V>(mut self, k: K, v: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
    {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
            self.headers.insert(k, v);
        }
        self
    }

    pub fn body<B: Into<Vec<u8>>>(mut self, body: B) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn json<T: Serialize + ?Sized>(mut self, v: &T) -> Self {
        match serde_json::to_vec(v) {
            Ok(b) => {
                self.headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                self.body = Some(b);
            }
            Err(e) => self.url = Err(Error::new(format!("json: {e}"))),
        }
        self
    }

    pub async fn send(self) -> Result<Response, Error> {
        let url = self.url?;
        let fut = exchange(self.method, url, self.headers, self.body);
        match self.timeout {
            Some(t) => tokio::time::timeout(t, fut)
                .await
                .map_err(|_| Error::new(format!("timeout after {t:?}")))?,
            None => fut.await,
        }
    }
}

pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    url: Url,
    body: bytes::Bytes,
}

impl Response {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub async fn bytes(self) -> Result<bytes::Bytes, Error> {
        Ok(self.body)
    }

    pub async fn text(self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.body).into_owned())
    }

    pub async fn json<T: DeserializeOwned>(self) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|e| Error::new(format!("json: {e}")))
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

async fn exchange(
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
) -> Result<Response, Error> {
    let https = match url.scheme() {
        "https" => true,
        "http" => false,
        s => return Err(Error::new(format!("unsupported scheme {s}"))),
    };
    let host = url.host_str().ok_or_else(|| Error::new("url has no host"))?.to_string();
    let port = url.port_or_known_default().ok_or_else(|| Error::new("url has no port"))?;

    let mut target = url.path().to_string();
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    let host_header = match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    let body = body.unwrap_or_default();
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nUser-Agent: sia-storage-wasi\r\n"
    );
    if !body.is_empty() || matches!(method, Method::POST | Method::PUT) {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (k, v) in headers.iter() {
        if let Ok(v) = v.to_str() {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str("\r\n");

    let tcp = net::connect(&host, port).await?;
    let raw = if https {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| Error::new(format!("server name {host}: {e}")))?;
        let mut tls = tokio_rustls::TlsConnector::from(tls_config())
            .connect(name, tcp)
            .await?;
        roundtrip(&mut tls, head.as_bytes(), &body).await?
    } else {
        let mut tcp = tcp;
        roundtrip(&mut tcp, head.as_bytes(), &body).await?
    };
    parse_response(url, &raw)
}

async fn roundtrip<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    s: &mut S,
    head: &[u8],
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    s.write_all(head).await?;
    if !body.is_empty() {
        s.write_all(body).await?;
    }
    s.flush().await?;
    let mut raw = Vec::new();
    // Connection: close — the body ends at EOF. A TLS peer that closes
    // without close_notify surfaces as UnexpectedEof once the bytes are in.
    match s.read_to_end(&mut raw).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => {}
        Err(e) => return Err(e.into()),
    }
    Ok(raw)
}

fn parse_response(url: Url, raw: &[u8]) -> Result<Response, Error> {
    let mut hdrs = [httparse::EMPTY_HEADER; 96];
    let mut resp = httparse::Response::new(&mut hdrs);
    let n = match resp.parse(raw).map_err(|e| Error::new(format!("response: {e}")))? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => return Err(Error::new("truncated response head")),
    };
    let status = StatusCode::from_u16(resp.code.unwrap_or(0))
        .map_err(|e| Error::new(format!("status: {e}")))?;
    let mut headers = HeaderMap::new();
    let mut chunked = false;
    let mut length = None;
    for h in resp.headers.iter() {
        let (Ok(k), Ok(v)) = (HeaderName::try_from(h.name), HeaderValue::from_bytes(h.value)) else {
            continue;
        };
        if k == http::header::TRANSFER_ENCODING
            && v.to_str().is_ok_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            chunked = true;
        }
        if k == http::header::CONTENT_LENGTH {
            length = v.to_str().ok().and_then(|v| v.trim().parse::<usize>().ok());
        }
        headers.append(k, v);
    }
    let rest = &raw[n..];
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(len) = length {
        if rest.len() < len {
            return Err(Error::new(format!("body truncated: {} of {len} bytes", rest.len())));
        }
        rest[..len].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(Response { status, headers, url, body: body.into() })
}

fn dechunk(mut raw: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    loop {
        let eol = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| Error::new("bad chunk header"))?;
        let size_txt = std::str::from_utf8(&raw[..eol]).map_err(|_| Error::new("bad chunk size"))?;
        let size = usize::from_str_radix(size_txt.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| Error::new("bad chunk size"))?;
        raw = &raw[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if raw.len() < size + 2 {
            return Err(Error::new("truncated chunk"));
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

pub mod net {
    //! Outbound TCP for wasip2: through the enclave's SOCKS5 egress front when
    //! `ENCLAVE_EGRESS` is set, direct otherwise. Names are handed to the
    //! front unresolved (ATYP DOMAIN) so resolution happens where its SSRF
    //! checks live; IP literals go as ATYP v4/v6.

    use std::io;
    use std::net::IpAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    pub async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
        match std::env::var("ENCLAVE_EGRESS") {
            Ok(url) if !url.is_empty() => socks_connect(&url, host, port).await,
            _ => direct(host.trim_start_matches('[').trim_end_matches(']'), port).await,
        }
    }

    /// tokio resolves names on a blocking-pool thread, and wasip2 has no
    /// threads: resolve with std's (wasi:sockets/ip-name-lookup) inline,
    /// then try each address in turn.
    async fn direct(host: &str, port: u16) -> io::Result<TcpStream> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no addresses"));
        for a in addrs {
            match TcpStream::connect(a).await {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// `addr` in `host:port` form (IPv6 hosts bracketed), as Sia host
    /// announcements carry it.
    pub async fn connect_addr(addr: &str) -> io::Result<TcpStream> {
        let (host, port) = addr
            .rsplit_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("no port in {addr}")))?;
        let port: u16 = port
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("bad port in {addr}")))?;
        connect(host.trim_start_matches('[').trim_end_matches(']'), port).await
    }

    fn bad(msg: String) -> io::Error {
        io::Error::other(msg)
    }

    async fn socks_connect(url: &str, host: &str, port: u16) -> io::Result<TcpStream> {
        let rest = url.split_once("://").map(|(_, r)| r).ok_or_else(|| bad("bad ENCLAVE_EGRESS url".into()))?;
        let (creds, front) = rest.rsplit_once('@').ok_or_else(|| bad("no credential in ENCLAVE_EGRESS".into()))?;
        let (id, token) = creds.split_once(':').ok_or_else(|| bad("bad ENCLAVE_EGRESS credential".into()))?;

        let (fh, fp) = front.rsplit_once(':').ok_or_else(|| bad("egress front has no port".into()))?;
        let mut s = direct(fh, fp.parse().map_err(|_| bad("bad egress front port".into()))?).await?;
        s.write_all(&[0x05, 0x01, 0x02]).await?;
        let mut m = [0u8; 2];
        s.read_exact(&mut m).await?;
        if m != [0x05, 0x02] {
            return Err(bad("egress front rejected user/pass auth".into()));
        }
        let mut auth = vec![0x01, id.len() as u8];
        auth.extend_from_slice(id.as_bytes());
        auth.push(token.len() as u8);
        auth.extend_from_slice(token.as_bytes());
        s.write_all(&auth).await?;
        let mut a = [0u8; 2];
        s.read_exact(&mut a).await?;
        if a[1] != 0x00 {
            return Err(bad("egress credential rejected".into()));
        }

        let mut req = vec![0x05, 0x01, 0x00];
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => {
                req.push(0x01);
                req.extend_from_slice(&v4.octets());
            }
            Ok(IpAddr::V6(v6)) => {
                req.push(0x04);
                req.extend_from_slice(&v6.octets());
            }
            Err(_) => {
                req.push(0x03);
                req.push(host.len() as u8);
                req.extend_from_slice(host.as_bytes());
            }
        }
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await?;
        if head[1] != 0x00 {
            return Err(bad(format!("egress CONNECT {host}:{port} refused (rep={})", head[1])));
        }
        let bnd_len = match head[3] {
            0x01 => 4,
            0x04 => 16,
            0x03 => {
                let mut l = [0u8; 1];
                s.read_exact(&mut l).await?;
                l[0] as usize
            }
            t => return Err(bad(format!("egress: unexpected BND.ADDR type {t}"))),
        };
        let mut skip = vec![0u8; bnd_len + 2];
        s.read_exact(&mut skip).await?;
        Ok(s)
    }
}
