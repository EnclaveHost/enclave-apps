//! ENet's platform layer, in Rust, over wasi:sockets.
//!
//! ENet splits cleanly into a portable protocol core and a platform layer. The
//! core (`vendor/enet/*.c`) is Moonlight's fork vendored verbatim, so the wire
//! format matches the client by construction rather than by reimplementation.
//! The platform layer is `unix.c` — BSD sockets, `poll`, `gettimeofday` — and
//! none of that crosses into a wasm guest.
//!
//! So the core is compiled for wasm and these thirteen symbols are supplied
//! from Rust instead, where `std::net::UdpSocket` maps onto wasi:sockets. The C
//! never learns the difference.
//!
//! Everything here is non-blocking. The caller is the loop that steps the
//! emulator, so `enet_socket_wait` returns immediately rather than sleeping —
//! the host is serviced by polling, not by a thread parked in `select`.
#![allow(non_camel_case_types)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

pub type ENetSocket = c_int;
pub type socklen_t = c_uint;

/// `struct { socklen_t addressLength; struct sockaddr_storage address; }`.
/// The fork carries the sockaddr inline, so size and alignment are load
/// bearing — this must agree with vendor/enet/include/enet/wasi.h byte for
/// byte or the C and Rust sides disagree about where the port is.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct ENetAddress {
    pub address_length: socklen_t,
    pub address: [u8; 128],
}

#[repr(C)]
pub struct ENetBuffer {
    pub data: *mut c_void,
    pub data_length: usize,
}

const AF_INET: u16 = 2;

/// The socket table. ENet addresses sockets by small integer, so hand out
/// indices and keep the real socket here.
fn table() -> &'static Mutex<Vec<Option<UdpSocket>>> {
    static T: OnceLock<Mutex<Vec<Option<UdpSocket>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}

/// Read a sockaddr_in out of an ENetAddress.
fn to_socket_addr(a: &ENetAddress) -> Option<SocketAddr> {
    let b = &a.address;
    let family = u16::from_ne_bytes([b[0], b[1]]);
    if family != AF_INET {
        return None;
    }
    // sin_port and sin_addr are network order on the wire.
    let port = u16::from_be_bytes([b[2], b[3]]);
    let ip = Ipv4Addr::new(b[4], b[5], b[6], b[7]);
    Some(SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

/// Write a sockaddr_in into an ENetAddress.
fn from_socket_addr(addr: SocketAddr, out: &mut ENetAddress) {
    out.address = [0u8; 128];
    let v4 = match addr {
        SocketAddr::V4(v4) => v4,
        // ENet's GameStream use is IPv4; a v6 peer is mapped rather than
        // dropped so the port at least survives.
        SocketAddr::V6(v6) => SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, v6.port()),
    };
    out.address[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    out.address[2..4].copy_from_slice(&v4.port().to_be_bytes());
    out.address[4..8].copy_from_slice(&v4.ip().octets());
    out.address_length = 16;
}

// --- the thirteen symbols the core calls -----------------------------------

#[no_mangle]
pub extern "C" fn enet_initialize() -> c_int {
    let _ = epoch();
    0
}

#[no_mangle]
pub extern "C" fn enet_deinitialize() {}

/// Seeds ENet's connection ids. Uses the monotonic clock rather than a random
/// device: this picks connect ids, not keys.
#[no_mangle]
pub extern "C" fn enet_host_random_seed() -> c_uint {
    epoch().elapsed().as_nanos() as c_uint
}

#[no_mangle]
pub extern "C" fn enet_time_get() -> c_uint {
    epoch().elapsed().as_millis() as c_uint
}

#[no_mangle]
pub extern "C" fn enet_time_set(_new_base: c_uint) {}

#[no_mangle]
pub extern "C" fn enet_address_equal(a: *const ENetAddress, b: *const ENetAddress) -> c_int {
    if a.is_null() || b.is_null() {
        return 0;
    }
    let (a, b) = unsafe { (&*a, &*b) };
    // Compare the meaningful prefix (family, port, address), not the padding.
    (a.address[..8] == b.address[..8]) as c_int
}

#[no_mangle]
pub extern "C" fn enet_address_wildcard(a: *const ENetAddress) -> c_int {
    if a.is_null() {
        return 1;
    }
    let a = unsafe { &*a };
    (a.address[4..8] == [0, 0, 0, 0]) as c_int
}

/// ENet creates a socket and binds it in two steps; `UdpSocket` has no
/// unbound form, so the slot is reserved here and filled by `bind`.
#[no_mangle]
pub extern "C" fn enet_socket_create(_af: c_int, _ty: c_int) -> ENetSocket {
    let mut t = table().lock().unwrap();
    t.push(None);
    (t.len() - 1) as ENetSocket
}

#[no_mangle]
pub extern "C" fn enet_socket_bind(socket: ENetSocket, address: *const ENetAddress) -> c_int {
    let addr = if address.is_null() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        to_socket_addr(unsafe { &*address }).unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)))
    };
    let sock = match UdpSocket::bind(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[enet] bind {addr} failed: {e}");
            return -1;
        }
    };
    if sock.set_nonblocking(true).is_err() {
        return -1;
    }
    let mut t = table().lock().unwrap();
    match t.get_mut(socket as usize) {
        Some(slot) => {
            *slot = Some(sock);
            0
        }
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn enet_socket_get_address(
    socket: ENetSocket,
    address: *mut ENetAddress,
) -> c_int {
    if address.is_null() {
        return -1;
    }
    let t = table().lock().unwrap();
    let Some(Some(s)) = t.get(socket as usize) else { return -1 };
    let Ok(local) = s.local_addr() else { return -1 };
    from_socket_addr(local, unsafe { &mut *address });
    0
}

/// Socket options ENet sets are all either the default here (non-blocking) or
/// buffer sizes wasi:sockets does not expose. Accepting them keeps the core's
/// error paths quiet about things that do not apply.
#[no_mangle]
pub extern "C" fn enet_socket_set_option(
    _socket: ENetSocket,
    _option: c_int,
    _value: c_int,
) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn enet_socket_get_option(
    _socket: ENetSocket,
    _option: c_int,
    value: *mut c_int,
) -> c_int {
    if !value.is_null() {
        unsafe { *value = 0 };
    }
    0
}

#[no_mangle]
pub extern "C" fn enet_socket_destroy(socket: ENetSocket) {
    if let Some(slot) = table().lock().unwrap().get_mut(socket as usize) {
        *slot = None;
    }
}

/// Gather-send. ENet hands an array of buffers; wasi:sockets takes one, so
/// flatten. GameStream datagrams are one MTU, so this is a small copy.
#[no_mangle]
pub extern "C" fn enet_socket_send(
    socket: ENetSocket,
    address: *const ENetAddress,
    buffers: *const ENetBuffer,
    buffer_count: usize,
) -> c_int {
    let t = table().lock().unwrap();
    let Some(Some(s)) = t.get(socket as usize) else { return -1 };
    let Some(peer) = (unsafe { address.as_ref() }).and_then(to_socket_addr) else {
        return -1;
    };
    let mut flat: Vec<u8> = Vec::with_capacity(1500);
    for i in 0..buffer_count {
        let b = unsafe { &*buffers.add(i) };
        if b.data.is_null() || b.data_length == 0 {
            continue;
        }
        flat.extend_from_slice(unsafe {
            std::slice::from_raw_parts(b.data as *const u8, b.data_length)
        });
    }
    match s.send_to(&flat, peer) {
        Ok(n) => n as c_int,
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
        Err(_) => -1,
    }
}

/// Scatter-receive. One datagram into the first buffer; ENet sizes it to an
/// MTU, and a datagram that does not fit is a datagram we could not have used.
#[no_mangle]
pub extern "C" fn enet_socket_receive(
    socket: ENetSocket,
    address: *mut ENetAddress,
    buffers: *mut ENetBuffer,
    buffer_count: usize,
) -> c_int {
    if buffer_count == 0 {
        return 0;
    }
    let t = table().lock().unwrap();
    let Some(Some(s)) = t.get(socket as usize) else { return -1 };
    let b = unsafe { &mut *buffers };
    if b.data.is_null() || b.data_length == 0 {
        return 0;
    }
    let out = unsafe { std::slice::from_raw_parts_mut(b.data as *mut u8, b.data_length) };
    match s.recv_from(out) {
        Ok((n, peer)) => {
            if !address.is_null() {
                from_socket_addr(peer, unsafe { &mut *address });
            }
            n as c_int
        }
        // No datagram waiting is the ordinary case for a polled host: 0, not
        // an error, or the core tears the peer down.
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
        Err(_) => -1,
    }
}

/// Never sleeps. The host is polled from the turn that steps the CPU, so
/// blocking here would stall the emulated machine; report "readable" and let
/// the core's receive find an empty socket.
#[no_mangle]
pub extern "C" fn enet_socket_wait(
    _socket: ENetSocket,
    condition: *mut c_uint,
    _timeout: c_uint,
) -> c_int {
    if !condition.is_null() {
        unsafe { *condition = 1 }; // ENET_SOCKET_WAIT_RECEIVE
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C and Rust sides share this struct by layout, not by declaration.
    /// If it drifts, ENet reads the port from the wrong offset and every
    /// connection silently goes nowhere.
    #[test]
    fn the_address_struct_matches_the_c_layout() {
        assert_eq!(std::mem::size_of::<ENetAddress>(), 136);
        assert_eq!(std::mem::align_of::<ENetAddress>(), 8);
    }

    /// Port and address are network order inside the sockaddr; getting that
    /// wrong is the classic byte-swap bug that only shows up on the wire.
    #[test]
    fn addresses_round_trip_in_network_order() {
        let mut a = ENetAddress { address_length: 0, address: [0; 128] };
        let want: SocketAddr = "192.168.8.249:47999".parse().unwrap();
        from_socket_addr(want, &mut a);
        assert_eq!(a.address_length, 16);
        assert_eq!(a.address[2..4], 47999u16.to_be_bytes(), "port must be big-endian");
        assert_eq!(a.address[4..8], [192, 168, 8, 249]);
        assert_eq!(to_socket_addr(&a), Some(want));
    }

    #[test]
    fn a_wildcard_address_is_recognised() {
        let mut a = ENetAddress { address_length: 0, address: [0; 128] };
        from_socket_addr("0.0.0.0:47999".parse().unwrap(), &mut a);
        assert_eq!(enet_address_wildcard(&a), 1);
        from_socket_addr("10.0.0.1:47999".parse().unwrap(), &mut a);
        assert_eq!(enet_address_wildcard(&a), 0);
    }

    /// Equality compares the meaningful prefix, so two addresses that differ
    /// only in padding are still the same peer.
    #[test]
    fn equality_ignores_the_padding() {
        let mut a = ENetAddress { address_length: 0, address: [0; 128] };
        let mut b = ENetAddress { address_length: 0, address: [0; 128] };
        let addr: SocketAddr = "10.1.2.3:1234".parse().unwrap();
        from_socket_addr(addr, &mut a);
        from_socket_addr(addr, &mut b);
        b.address[100] = 0xff;
        assert_eq!(enet_address_equal(&a, &b), 1);
        from_socket_addr("10.1.2.4:1234".parse().unwrap(), &mut b);
        assert_eq!(enet_address_equal(&a, &b), 0);
    }

    /// Binding must actually produce a usable socket and report its address.
    #[test]
    fn a_socket_binds_and_reports_its_address() {
        let s = enet_socket_create(2, 0);
        let mut a = ENetAddress { address_length: 0, address: [0; 128] };
        from_socket_addr("127.0.0.1:0".parse().unwrap(), &mut a);
        assert_eq!(enet_socket_bind(s, &a), 0, "bind must succeed");
        let mut got = ENetAddress { address_length: 0, address: [0; 128] };
        assert_eq!(enet_socket_get_address(s, &mut got), 0);
        let addr = to_socket_addr(&got).expect("a bound socket has an address");
        assert_ne!(addr.port(), 0, "the kernel must have assigned a port");
        enet_socket_destroy(s);
    }
}
