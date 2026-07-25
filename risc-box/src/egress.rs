//! Fleet egress: dial out through the per-deployment SOCKS5 front when the
//! platform provides one, fall back to a direct connect when it doesn't.
//!
//! On an egress-enabled enclave the app has no raw outbound network — every
//! connect must go through the front named by `ENCLAVE_EGRESS`
//! (`socks5://<id>:<token>@host:port`, the per-deployment credential the
//! wasm-manager injects). Locally under `wasmtime -Sinherit-network` the
//! variable is absent and `dial()` is just `TcpStream::connect`. The
//! handshake mirrors the platform's reference client (network-test app):
//! user/pass subnegotiation (id/token), then CONNECT; BND.ADDR (the
//! deployment's dedicated address) is drained and discarded here.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

/// Connect to `host:port` — through the SOCKS front when one is configured.
/// `host` may be a name (S3 endpoint) or an IP in text form (guest NAT).
pub fn dial(host: &str, port: u16, timeout: Option<Duration>) -> Result<TcpStream, String> {
    match std::env::var("ENCLAVE_EGRESS") {
        Ok(url) if !url.is_empty() => socks_connect(&url, host, port, timeout),
        _ => direct(host, port, timeout),
    }
}

fn direct(host: &str, port: u16, timeout: Option<Duration>) -> Result<TcpStream, String> {
    match timeout {
        Some(t) => {
            // connect_timeout wants a resolved SocketAddr; NAT hands us IPs
            let ip: IpAddr = host
                .parse()
                .map_err(|_| format!("connect_timeout needs an IP, got {host}"))?;
            TcpStream::connect_timeout(&SocketAddr::new(ip, port), t)
        }
        None => TcpStream::connect((host, port)),
    }
    .map_err(|e| format!("connect {host}:{port}: {e}"))
}

fn socks_connect(url: &str, host: &str, port: u16, timeout: Option<Duration>) -> Result<TcpStream, String> {
    // socks5://id:token@front_host:front_port
    let rest = url.split_once("://").map(|(_, r)| r).ok_or("bad ENCLAVE_EGRESS url")?;
    let (creds, front) = rest.rsplit_once('@').ok_or("no credential in ENCLAVE_EGRESS")?;
    let (id, token) = creds.split_once(':').ok_or("bad ENCLAVE_EGRESS credential")?;

    let mut s = TcpStream::connect(front).map_err(|e| format!("egress front {front}: {e}"))?;
    let hs_timeout = timeout.unwrap_or(Duration::from_secs(20));
    let _ = s.set_read_timeout(Some(hs_timeout));

    // greeting: we offer user/pass auth only
    s.write_all(&[0x05, 0x01, 0x02]).map_err(es)?;
    let mut m = [0u8; 2];
    s.read_exact(&mut m).map_err(es)?;
    if m != [0x05, 0x02] {
        return Err("egress front rejected user/pass auth".into());
    }
    let mut auth = vec![0x01, id.len() as u8];
    auth.extend_from_slice(id.as_bytes());
    auth.push(token.len() as u8);
    auth.extend_from_slice(token.as_bytes());
    s.write_all(&auth).map_err(es)?;
    let mut a = [0u8; 2];
    s.read_exact(&mut a).map_err(es)?;
    if a[1] != 0x00 {
        return Err("egress credential rejected".into());
    }

    // CONNECT: real IPs go as ATYP v4/v6, names as ATYP DOMAIN (the front
    // resolves them where its SSRF checks live)
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
    s.write_all(&req).map_err(es)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).map_err(es)?;
    if head[1] != 0x00 {
        return Err(format!("egress CONNECT {host}:{port} refused (rep={})", head[1]));
    }
    // drain BND.ADDR + BND.PORT
    let bnd_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).map_err(es)?;
            l[0] as usize
        }
        t => return Err(format!("egress: unexpected BND.ADDR type {t}")),
    };
    let mut skip = vec![0u8; bnd_len + 2];
    s.read_exact(&mut skip).map_err(es)?;
    let _ = s.set_read_timeout(None);
    Ok(s)
}

fn es(e: std::io::Error) -> String {
    format!("egress front i/o: {e}")
}
