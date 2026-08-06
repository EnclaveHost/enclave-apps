// Minimal FFI surface for the vendored ENet (Moonlight's fork).
//
// We only need the server side: create a host, service events, receive
// packets, send packets, disconnect. The client is moonlight-common-c linked
// against this same fork, so the wire format matches by construction.
//
// NB: this fork replaced upstream's `enet_uint32 host` with a sockaddr_storage
// (vendor/enet/include/enet/enet.h), so ENetAddress is NOT layout-compatible
// with stock ENet bindings.

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_void};

pub const AF_INET: c_int = 2;

pub const ENET_EVENT_TYPE_NONE: c_int = 0;
pub const ENET_EVENT_TYPE_CONNECT: c_int = 1;
pub const ENET_EVENT_TYPE_DISCONNECT: c_int = 2;
pub const ENET_EVENT_TYPE_RECEIVE: c_int = 3;

pub const ENET_PACKET_FLAG_RELIABLE: u32 = 1 << 0;
pub const ENET_PACKET_FLAG_UNSEQUENCED: u32 = 1 << 1;

/// `struct { socklen_t addressLength; struct sockaddr_storage address; }`.
/// sockaddr_storage is 128 bytes with 8-byte alignment on Linux, which puts
/// the sockaddr at offset 8.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct ENetAddress {
    pub address_length: u32,
    _pad: u32,
    pub address: [u8; 128],
}

impl Default for ENetAddress {
    fn default() -> Self {
        ENetAddress { address_length: 0, _pad: 0, address: [0u8; 128] }
    }
}

impl ENetAddress {
    /// An IPv4 `sockaddr_in` for 0.0.0.0:port — what a server binds to.
    pub fn any_v4(port: u16) -> ENetAddress {
        let mut a = ENetAddress::default();
        a.address_length = 16; // sizeof(struct sockaddr_in)
        a.address[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
        a.address[2..4].copy_from_slice(&port.to_be_bytes()); // sin_port, network order
        // sin_addr stays 0.0.0.0, sin_zero stays zero.
        a
    }
}

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
    pub fn enet_initialize() -> c_int;
    pub fn enet_deinitialize();

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

    pub fn enet_packet_create(data: *const c_void, data_length: usize, flags: u32) -> *mut ENetPacket;
    pub fn enet_packet_destroy(packet: *mut ENetPacket);

    pub fn enet_peer_send(peer: *mut c_void, channel_id: u8, packet: *mut ENetPacket) -> c_int;
    pub fn enet_peer_disconnect(peer: *mut c_void, data: u32);
    pub fn enet_peer_disconnect_now(peer: *mut c_void, data: u32);
    pub fn enet_peer_reset(peer: *mut c_void);
    pub fn enet_peer_timeout(peer: *mut c_void, limit: u32, minimum: u32, maximum: u32);
}

/// Bring up the ENet library once for the process.
pub fn init() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        if enet_initialize() != 0 {
            panic!("enet_initialize() failed");
        }
    });
}

/// Send `data` to `peer` on `channel`, reliably. Returns false if ENet
/// refused the packet (peer gone, queue full).
pub fn send_reliable(peer: *mut c_void, channel: u8, data: &[u8]) -> bool {
    unsafe {
        let pkt = enet_packet_create(
            data.as_ptr() as *const c_void,
            data.len(),
            ENET_PACKET_FLAG_RELIABLE,
        );
        if pkt.is_null() {
            return false;
        }
        if enet_peer_send(peer, channel, pkt) < 0 {
            enet_packet_destroy(pkt);
            return false;
        }
        true
    }
}
