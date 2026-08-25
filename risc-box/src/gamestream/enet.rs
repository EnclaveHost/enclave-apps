//! The control channel's transport boundary.
//!
//! GameStream's control stream (input events, IDR requests, termination) rides
//! ENet, and the client is moonlight-common-c linked against Moonlight's ENet
//! fork — so the wire format has to match that fork exactly, not ENet-in-general.
//!
//! The native bridge got this for free by linking the same C: 6,100 lines of
//! vendored ENet built with `cc`. That does not cross the sandbox. ENet's
//! protocol core (host/peer/protocol/packet/list) is portable, but its platform
//! layer (`unix.c`, 27 functions) is BSD sockets and `poll`, and a wasm guest
//! reaches the network through wasi:sockets instead. Bringing it over means
//! vendoring the protocol core and writing a WASI socket layer beneath it, or
//! reimplementing the protocol here — neither of which is a wrapper around what
//! already exists.
//!
//! Until then this is an explicit boundary rather than a stub that pretends:
//! datagrams arriving on 47999 are counted and dropped, and everything above
//! (discovery, pairing, launch, RTSP, RTP video) works without them. What a
//! client loses is input and on-demand IDR — it can watch, not play.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::gamestream::session::Session;

static SEEN: AtomicU64 = AtomicU64::new(0);
static WARNED: AtomicU64 = AtomicU64::new(0);

/// One datagram from the control port.
///
/// Logged once and then every 500th, because a connecting client retries hard
/// and an unthrottled line would bury the log it is meant to explain.
pub fn on_datagram(_session: &Arc<Session>, _sock: &UdpSocket, peer: SocketAddr, data: &[u8]) {
    let n = SEEN.fetch_add(1, Ordering::Relaxed);
    if n == 0 || n % 500 == 0 {
        let last = WARNED.swap(n, Ordering::Relaxed);
        let _ = last;
        eprintln!(
            "[control] {} ENet datagram(s) from {peer} ({} bytes) - the control channel is \
             not implemented in-guest yet, so input and IDR requests are dropped",
            n + 1,
            data.len()
        );
    }
}

/// How many control datagrams have arrived. A non-zero count with no video
/// moving means the client reached us and we could not answer it.
pub fn datagrams_seen() -> u64 {
    SEEN.load(Ordering::Relaxed)
}
