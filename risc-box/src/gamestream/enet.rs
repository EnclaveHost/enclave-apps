//! The GameStream control channel, over Moonlight's ENet fork.
//!
//! The client is moonlight-common-c linked against this exact fork, so the
//! protocol core is vendored and compiled verbatim (`vendor/enet/`) rather than
//! reimplemented: a reimplementation that is 99% right is a control channel
//! that connects and then stalls. What could not come across is ENet's platform
//! layer — BSD sockets and `poll` — so that is supplied from Rust over
//! wasi:sockets in [`super::enet_sys`].
//!
//! ENet owns udp/47999 itself: it binds the socket through the platform layer,
//! and this module drives it by calling `enet_host_service` with a zero timeout
//! from the app's turn. Nothing blocks — the caller is the loop that steps the
//! emulated CPU.
#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_void};

use super::enet_sys::ENetAddress;

pub const AF_INET: c_int = 2;

pub const ENET_EVENT_TYPE_NONE: c_int = 0;
pub const ENET_EVENT_TYPE_CONNECT: c_int = 1;
pub const ENET_EVENT_TYPE_DISCONNECT: c_int = 2;
pub const ENET_EVENT_TYPE_RECEIVE: c_int = 3;

pub const ENET_PACKET_FLAG_RELIABLE: u32 = 1 << 0;

#[repr(C)]
pub struct ENetPacket {
    pub reference_count: usize,
    pub flags: u32,
    pub data: *mut u8,
    pub data_length: usize,
    pub free_callback: *mut c_void,
    pub user_data: *mut c_void,
}

#[repr(C)]
pub struct ENetEvent {
    pub kind: c_int,
    pub peer: *mut c_void,
    pub channel_id: u8,
    pub data: u32,
    pub packet: *mut ENetPacket,
}

impl Default for ENetEvent {
    fn default() -> Self {
        ENetEvent {
            kind: ENET_EVENT_TYPE_NONE,
            peer: std::ptr::null_mut(),
            channel_id: 0,
            data: 0,
            packet: std::ptr::null_mut(),
        }
    }
}

extern "C" {
    pub fn enet_host_create(
        address_family: c_int,
        address: *const ENetAddress,
        peer_count: usize,
        channel_limit: usize,
        incoming_bandwidth: u32,
        outgoing_bandwidth: u32,
    ) -> *mut c_void;
    pub fn enet_host_destroy(host: *mut c_void);
    pub fn enet_host_service(host: *mut c_void, event: *mut ENetEvent, timeout: u32) -> c_int;
    pub fn enet_host_flush(host: *mut c_void);

    pub fn enet_packet_create(
        data: *const c_void,
        data_length: usize,
        flags: u32,
    ) -> *mut ENetPacket;
    pub fn enet_packet_destroy(packet: *mut ENetPacket);

    pub fn enet_peer_send(peer: *mut c_void, channel_id: u8, packet: *mut ENetPacket) -> c_int;
    pub fn enet_peer_disconnect_now(peer: *mut c_void, data: u32);
    pub fn enet_peer_timeout(peer: *mut c_void, limit: u32, minimum: u32, maximum: u32);
}

/// What one turn of the control channel produced.
pub enum Event {
    Connected,
    /// A control message from the client, on `channel`.
    Message { channel: u8, data: Vec<u8> },
    Disconnected,
}

/// A polled ENet server host.
pub struct Host {
    host: *mut c_void,
    peer: *mut c_void,
}

// The host is only ever touched from the app's single turn.
unsafe impl Send for Host {}

impl Host {
    /// Bind the control port. `peers` is 1: GameStream is one client at a time.
    pub fn bind(port: u16) -> Option<Host> {
        let mut addr = ENetAddress { address_length: 16, address: [0u8; 128] };
        addr.address[0..2].copy_from_slice(&2u16.to_ne_bytes()); // AF_INET
        addr.address[2..4].copy_from_slice(&port.to_be_bytes()); // network order
        // address stays 0.0.0.0 — bind on every interface.

        let host = unsafe { enet_host_create(AF_INET, &addr, 1, 1, 0, 0) };
        if host.is_null() {
            eprintln!("[control] enet_host_create failed on udp/{port}");
            return None;
        }
        eprintln!("[control] ENet listening on :{port}");
        Some(Host { host, peer: std::ptr::null_mut() })
    }

    /// Drain the host. Zero timeout: this runs inside the turn that steps the
    /// CPU, so it must never wait.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let mut ev = ENetEvent::default();
            let rc = unsafe { enet_host_service(self.host, &mut ev, 0) };
            if rc <= 0 {
                break;
            }
            match ev.kind {
                ENET_EVENT_TYPE_CONNECT => {
                    self.peer = ev.peer;
                    // Moonlight expects a host that gives up on a silent peer
                    // reasonably fast; the defaults are minutes.
                    unsafe { enet_peer_timeout(ev.peer, 32, 5_000, 10_000) };
                    out.push(Event::Connected);
                }
                ENET_EVENT_TYPE_RECEIVE => {
                    if !ev.packet.is_null() {
                        let p = unsafe { &*ev.packet };
                        let data = unsafe {
                            std::slice::from_raw_parts(p.data, p.data_length)
                        }
                        .to_vec();
                        out.push(Event::Message { channel: ev.channel_id, data });
                        unsafe { enet_packet_destroy(ev.packet) };
                    }
                }
                ENET_EVENT_TYPE_DISCONNECT => {
                    self.peer = std::ptr::null_mut();
                    out.push(Event::Disconnected);
                }
                _ => {}
            }
        }
        out
    }

    /// Send reliably to the connected client. False if there is no peer or
    /// ENet refused the packet.
    pub fn send(&self, channel: u8, data: &[u8]) -> bool {
        if self.peer.is_null() {
            return false;
        }
        unsafe {
            let pkt = enet_packet_create(
                data.as_ptr() as *const c_void,
                data.len(),
                ENET_PACKET_FLAG_RELIABLE,
            );
            if pkt.is_null() {
                return false;
            }
            if enet_peer_send(self.peer, channel, pkt) < 0 {
                enet_packet_destroy(pkt);
                return false;
            }
            enet_host_flush(self.host);
            true
        }
    }

    pub fn connected(&self) -> bool {
        !self.peer.is_null()
    }

    /// Drop the client immediately — used when the session ends.
    pub fn disconnect(&mut self) {
        if !self.peer.is_null() {
            unsafe { enet_peer_disconnect_now(self.peer, 0) };
            self.peer = std::ptr::null_mut();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.disconnect();
        if !self.host.is_null() {
            unsafe { enet_host_destroy(self.host) };
            self.host = std::ptr::null_mut();
        }
    }
}
