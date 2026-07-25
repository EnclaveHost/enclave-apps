//! irc — an IRC server compiled to a wasm32-wasip2 command component,
//! written to run as an Enclave "service app" (`wasmtime run` + wasi:sockets).
//!
//! The platform contract (ENCLAVE_PORTS; see the enclave repo wasm-manager docs):
//!   - The manager launches us with `wasmtime run -Stcp -Sudp -Sinherit-network
//!     -Sallow-ip-name-lookup --env ENCLAVE_PORTS=...`.
//!   - `ENCLAVE_PORTS` maps each LOGICAL declared port to the ACTUAL per-deployment
//!     bind, e.g. `tcp:6667=31245`. The one rule: bind the ACTUAL, never
//!     hardcode. (Locally, with no ENCLAVE_PORTS, we default to 6667.)
//!   - We bind loopback only; the supervisor's WebSocket bridge at
//!     /x/:id/tcp/6667 is the public data path.
//!   - No filesystem, no threads: one non-blocking event loop owns everything.

mod message;
mod server;

pub const NICK_MAX: usize = 24;
pub const CHANNEL_MAX: usize = 50;
pub const TOPIC_MAX: usize = 390;

/// The logical port we advertise in the deploy's firewall config.
const LOGICAL_PORT: u16 = 6667;

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::thread;
use std::time::Duration;

/// Resolve the port to bind from `ENCLAVE_PORTS` ("tcp:6667=31245,udp:9053=31246"):
/// prefer our logical entry, fall back to the first tcp entry, then to the
/// logical number itself (local development without a manager).
fn resolve_port() -> u16 {
    let Ok(spec) = std::env::var("ENCLAVE_PORTS") else {
        return LOGICAL_PORT;
    };
    let mut first_tcp = None;
    for entry in spec.split(',') {
        let Some((label, actual)) = entry.trim().split_once('=') else {
            continue;
        };
        let label = label.trim().to_ascii_lowercase();
        let Ok(port) = actual.trim().parse::<u16>() else {
            continue;
        };
        if label == format!("tcp:{}", LOGICAL_PORT) {
            return port;
        }
        if label.starts_with("tcp:") && first_tcp.is_none() {
            first_tcp = Some(port);
        }
    }
    match first_tcp {
        Some(p) => p,
        None => {
            eprintln!("ENCLAVE_PORTS={:?} has no usable tcp entry; falling back to {}", spec, LOGICAL_PORT);
            LOGICAL_PORT
        }
    }
}

fn main() {
    let port = resolve_port();
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fatal: cannot bind {}: {}", addr, e);
            std::process::exit(1);
        }
    };
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener is required");
    println!(
        "irc-{} listening on {} (logical tcp:{}; single-threaded wasi:sockets loop)",
        env!("CARGO_PKG_VERSION"),
        addr,
        LOGICAL_PORT
    );

    let mut srv = server::Server::new();
    loop {
        let mut busy = false;
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    srv.accept(stream, peer);
                    busy = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("accept error: {}", e);
                    break;
                }
            }
        }
        busy |= srv.pump();
        srv.tick();
        srv.flush();
        srv.reap();
        // Idle tick ~25ms keeps latency low without burning the CPU share;
        // under load we shorten the sleep so bursts drain quickly.
        thread::sleep(Duration::from_millis(if busy { 2 } else { 25 }));
    }
}
