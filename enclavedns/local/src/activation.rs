//! systemd socket activation (sd_listen_fds): systemd holds the ports, so the
//! browser never meets a closed port while this daemon starts or restarts.

use socket2::{Socket, Type};
use std::net::{TcpListener, UdpSocket};
use std::os::fd::FromRawFd;

pub struct Listeners {
    pub http: TcpListener,
    pub https: TcpListener,
    pub dns_tcp: TcpListener,
    pub dns_udp: UdpSocket,
}

/// The sockets systemd passed, told apart by type and port (TCP 443 is
/// HTTPS, TCP 53 DNS, any other TCP port HTTP, UDP DNS); None when not
/// socket-activated.
pub fn listeners() -> Result<Option<Listeners>, String> {
    let ours = std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let n: i32 = std::env::var("LISTEN_FDS").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
    if !ours || n == 0 {
        return Ok(None);
    }
    let (mut http, mut https, mut dns_tcp, mut dns_udp) = (None, None, None, None);
    for fd in 3..3 + n {
        // SAFETY: systemd hands these descriptors to this process, once
        let s = unsafe { Socket::from_raw_fd(fd) };
        let port = s.local_addr().ok().and_then(|a| a.as_socket()).map(|a| a.port());
        match (s.r#type().map_err(|e| e.to_string())?, port) {
            (Type::STREAM, Some(53)) => dns_tcp = Some(TcpListener::from(s)),
            (Type::STREAM, Some(443)) => https = Some(TcpListener::from(s)),
            (Type::STREAM, Some(_)) => http = Some(TcpListener::from(s)),
            (Type::DGRAM, _) => dns_udp = Some(UdpSocket::from(s)),
            (t, p) => return Err(format!("unexpected socket from systemd: {t:?} port {p:?}")),
        }
    }
    match (http, https, dns_tcp, dns_udp) {
        (Some(http), Some(https), Some(dns_tcp), Some(dns_udp)) => Ok(Some(Listeners { http, https, dns_tcp, dns_udp })),
        _ => Err("systemd must pass four sockets: TCP :80, TCP :443, TCP :53 and UDP :53".into()),
    }
}
