//! EnclaveDNS Local: opens `.eth` sites in an ordinary browser.
//!
//! A browser needs an IP address to open `playhh.eth`, but most ENS names
//! point at content instead (IPFS, IPNS, Swarm, Arweave, a Tor onion
//! service) or at nothing a browser can use. This daemon sits on its own
//! loopback address and does two jobs:
//!
//! * DNS for `.eth` (and the `.ens` alias): every question is asked of
//!   EnclaveDNS over DoH. Real records stored on-chain come back unchanged;
//!   a name that holds anything else gets this daemon's address for `A`, so
//!   the browser comes here.
//! * HTTP: the site is served from the LOCAL software for its protocol: a
//!   kubo node's gateway, a Bee node, an AR.IO gateway, or Tor. When that
//!   software is not running, the page says so and how to start it, instead
//!   of the browser's generic "can't be reached". A `url` text record
//!   becomes a redirect; a name with only an address gets a page showing it.
//!
//! It never falls back to a public gateway: the content comes from software
//! on this machine, and the name from EnclaveDNS.

mod activation;
mod dns;
mod ens;
mod gateway;
mod pages;
mod tls;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

pub struct Cfg {
    /// EnclaveDNS base URL (DoH at /dns-query, ENS at /api/ens/<name>)
    pub enclavedns: String,
    /// The address `.eth` names get when this daemon serves them.
    pub gateway_ip: Ipv4Addr,
    pub ipfs: String,
    /// kubo's RPC API, used only to resolve IPNS names quickly
    pub ipfs_api: String,
    pub bee: String,
    pub arweave: String,
    pub tor: SocketAddr,
    pub http_port: u16,
    pub https_port: u16,
    pub dns_port: u16,
    /// where the .eth CA lives (systemd's StateDirectory)
    pub state: std::path::PathBuf,
    /// send http:// .eth requests to https:// (once the CA is trusted)
    pub redirect_https: bool,
}

pub struct State {
    pub cfg: Cfg,
    pub ens: ens::Ens,
    pub backends: gateway::Backends,
}

fn usage() -> ! {
    eprintln!(
        "enclavedns-local [--enclavedns URL] [--listen IP] [--ipfs URL] [--bee URL] [--arweave URL] [--tor HOST:PORT]

  --enclavedns  EnclaveDNS to ask             (default https://59f1273d.app.enclave.host)
  --listen      this daemon's loopback IP     (default 127.0.0.3; DNS :53, HTTP :80, HTTPS :443)
  --ipfs        kubo gateway                  (default http://127.0.0.1:8080)
  --ipfs-api    kubo RPC API, for fast IPNS   (default http://127.0.0.1:5001)
  --bee         Bee (Swarm) node API          (default http://127.0.0.1:1633)
  --arweave     AR.IO gateway                 (default http://127.0.0.1:3000)
  --tor         Tor SOCKS port                (default 127.0.0.1:9050)
  --state       the .eth CA's directory       (default $STATE_DIRECTORY or /var/lib/enclavedns-local)
  --redirect-https  send http:// .eth requests to https:// (trust <state>/ca.crt first)
  --http-port, --https-port, --dns-port   without socket activation (defaults 80, 443, 53)

Under systemd socket activation the listening sockets come from systemd
(TCP on ports 80 and 443, TCP and UDP on port 53)."
    );
    std::process::exit(2)
}

fn parse_args() -> Cfg {
    let mut cfg = Cfg {
        enclavedns: "https://59f1273d.app.enclave.host".into(),
        gateway_ip: Ipv4Addr::new(127, 0, 0, 3),
        ipfs: "http://127.0.0.1:8080".into(),
        ipfs_api: "http://127.0.0.1:5001".into(),
        bee: "http://127.0.0.1:1633".into(),
        arweave: "http://127.0.0.1:3000".into(),
        tor: "127.0.0.1:9050".parse().unwrap(),
        http_port: 80,
        https_port: 443,
        dns_port: 53,
        state: std::env::var_os("STATE_DIRECTORY").map(Into::into).unwrap_or_else(|| "/var/lib/enclavedns-local".into()),
        redirect_https: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--enclavedns" => cfg.enclavedns = val().trim_end_matches('/').to_string(),
            "--listen" => cfg.gateway_ip = val().parse().unwrap_or_else(|_| usage()),
            "--ipfs" => cfg.ipfs = val().trim_end_matches('/').to_string(),
            "--ipfs-api" => cfg.ipfs_api = val().trim_end_matches('/').to_string(),
            "--bee" => cfg.bee = val().trim_end_matches('/').to_string(),
            "--arweave" => cfg.arweave = val().trim_end_matches('/').to_string(),
            "--tor" => cfg.tor = val().parse().unwrap_or_else(|_| usage()),
            "--http-port" => cfg.http_port = val().parse().unwrap_or_else(|_| usage()),
            "--dns-port" => cfg.dns_port = val().parse().unwrap_or_else(|_| usage()),
            "--https-port" => cfg.https_port = val().parse().unwrap_or_else(|_| usage()),
            "--state" => cfg.state = val().into(),
            "--redirect-https" => cfg.redirect_https = true,
            _ => usage(),
        }
    }
    cfg
}

#[tokio::main]
async fn main() {
    let cfg = parse_args();
    let socks = activation::listeners().unwrap_or_else(|e| {
        eprintln!("enclavedns-local: {e}");
        std::process::exit(1)
    });
    let l = match socks {
        Some(s) => s,
        None => {
            let ip = cfg.gateway_ip;
            let bind = |port| SocketAddr::from((ip, port));
            let r = (|| -> std::io::Result<activation::Listeners> {
                Ok(activation::Listeners {
                    http: std::net::TcpListener::bind(bind(cfg.http_port))?,
                    https: std::net::TcpListener::bind(bind(cfg.https_port))?,
                    dns_tcp: std::net::TcpListener::bind(bind(cfg.dns_port))?,
                    dns_udp: std::net::UdpSocket::bind(bind(cfg.dns_port))?,
                })
            })();
            r.unwrap_or_else(|e| {
                eprintln!("enclavedns-local: bind on {ip}: {e}");
                std::process::exit(1)
            })
        }
    };
    let ca = tls::Ca::load_or_create(&cfg.state).unwrap_or_else(|e| {
        eprintln!("enclavedns-local: the .eth CA: {e}");
        std::process::exit(1)
    });
    let tls_cfg = std::sync::Arc::new(ca).server_config();
    eprintln!(
        "enclavedns-local {}: .eth DNS and sites on {} (http, https), names from {}, CA {}",
        env!("CARGO_PKG_VERSION"),
        cfg.gateway_ip,
        cfg.enclavedns,
        cfg.state.join(tls::CA_CERT).display()
    );
    let st = Arc::new(State { ens: ens::Ens::new(&cfg.enclavedns), backends: gateway::Backends::default(), cfg });
    let tcp = |t: std::net::TcpListener| {
        t.set_nonblocking(true).expect("nonblocking");
        tokio::net::TcpListener::from_std(t).expect("listener")
    };
    l.dns_udp.set_nonblocking(true).expect("nonblocking");
    let dns_udp = tokio::net::UdpSocket::from_std(l.dns_udp).expect("dns udp socket");
    tokio::join!(
        gateway::serve(tcp(l.http), st.clone(), None),
        gateway::serve(tcp(l.https), st.clone(), Some(tokio_rustls::TlsAcceptor::from(tls_cfg))),
        dns::serve_tcp(tcp(l.dns_tcp), st.clone()),
        dns::serve_udp(dns_udp, st.clone())
    );
}
