// UDP ping listeners for the video and audio ports.
//
// The client does not tell us where to send media: it pings the video and
// audio ports every 500 ms and the host learns the peer address from those
// packets. Modern clients send an SS_PING — 16 bytes of payload (the
// X-SS-Ping-Payload we handed out in RTSP SETUP) plus a big-endian sequence
// number — while older ones send the 4-byte literal "PING".

use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::net::SocketAddr;

use crate::gamestream::session::Session;

/// Watch `sock` for client pings and record the peer address in `slot`.
pub fn watch(session: Arc<Session>, sock: Arc<UdpSocket>, slot: fn(&Session) -> &Mutex<Option<SocketAddr>>, label: &'static str) {
    let mut buf = [0u8; 2048];
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(500)));

    while !session.is_stopping() {
        let Ok((n, peer)) = sock.recv_from(&mut buf) else { continue };

        // Identify the ping: either our 16-byte payload, or the legacy literal.
        let matched = if n >= 16 {
            &buf[..16] == session.ping_payload.as_bytes()
        } else {
            n == 4 && &buf[..4] == b"PING"
        };
        if !matched {
            continue;
        }

        let mut cur = slot(&session).lock().unwrap();
        if *cur != Some(peer) {
            eprintln!("[{label}] client ping from {peer}");
            *cur = Some(peer);
        }
    }
}

pub fn video_slot(s: &Session) -> &Mutex<Option<SocketAddr>> {
    &s.video_peer
}

pub fn audio_slot(s: &Session) -> &Mutex<Option<SocketAddr>> {
    &s.audio_peer
}
