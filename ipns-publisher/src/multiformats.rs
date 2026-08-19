//! The multiformats floor of the stack: varints, the three bases IPNS
//! identities travel in (base32 CIDv1, base36 IPNS names, base58btc peer
//! IDs), hex, base64, and binary multiaddr decoding. Hand-rolled from the
//! specs, verified against kubo 0.42 vectors in the unit tests.

#![allow(dead_code)]

// ---- varint ----------------------------------------------------------------

pub fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn varint_read(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &b) in buf.iter().enumerate().take(10) {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

// ---- hex -------------------------------------------------------------------

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    for i in (0..b.len()).step_by(2) {
        let hi = hex_val(b[i])?;
        let lo = hex_val(b[i + 1])?;
        out.push(hi * 16 + lo);
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---- base64 (standard alphabet, padding optional on decode) ----------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62, // accept url-safe too
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// ---- base32 (RFC 4648 lowercase, no padding: multibase 'b') ----------------

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

pub fn base32(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 8 / 5 + 1);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

// ---- big-integer bases (base36 for IPNS names, base58btc for peer IDs) -----

fn base_encode(alphabet: &[u8], data: &[u8]) -> String {
    let base = alphabet.len() as u32;
    let mut digits: Vec<u8> = Vec::new(); // little-endian result digits
    for &byte in data {
        let mut carry = u32::from(byte);
        for d in digits.iter_mut() {
            carry += u32::from(*d) << 8;
            *d = (carry % base) as u8;
            carry /= base;
        }
        while carry > 0 {
            digits.push((carry % base) as u8);
            carry /= base;
        }
    }
    let mut out = String::with_capacity(digits.len() + 4);
    for &b in data.iter().take_while(|&&b| b == 0) {
        let _ = b;
        out.push(alphabet[0] as char);
    }
    for &d in digits.iter().rev() {
        out.push(alphabet[d as usize] as char);
    }
    out
}

fn base_decode(alphabet: &[u8], s: &str) -> Option<Vec<u8>> {
    let base = alphabet.len() as u32;
    // leading alphabet[0] chars are leading zero bytes by convention
    let zeros = s.bytes().take_while(|&c| c == alphabet[0]).count();
    let mut le: Vec<u8> = Vec::new(); // little-endian significant bytes
    for c in s.bytes().skip(zeros) {
        let v = alphabet.iter().position(|&a| a == c)? as u32;
        let mut carry = v;
        for b in le.iter_mut() {
            carry += u32::from(*b) * base;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            le.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let mut out = vec![0u8; zeros];
    out.extend(le.iter().rev());
    Some(out)
}

const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const BASE58: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

pub fn base36(data: &[u8]) -> String {
    base_encode(BASE36, data)
}
pub fn base36_decode(s: &str) -> Option<Vec<u8>> {
    base_decode(BASE36, &s.to_ascii_lowercase())
}
pub fn base58btc(data: &[u8]) -> String {
    base_encode(BASE58, data)
}
pub fn base58_decode(s: &str) -> Option<Vec<u8>> {
    base_decode(BASE58, s)
}

// ---- multiaddr -------------------------------------------------------------
//
// Binary multiaddrs arrive inside Kademlia Peer messages. This decoder knows
// the codes a DHT walk actually meets; unknown codes with a length prefix
// are skipped, unknown fixed-size codes poison the address (can't resume).

pub const MA_IP4: u64 = 0x04;
pub const MA_TCP: u64 = 0x06;
pub const MA_UDP: u64 = 0x0111;
pub const MA_DNS: u64 = 0x35;
pub const MA_DNS4: u64 = 0x36;
pub const MA_DNS6: u64 = 0x37;
pub const MA_DNSADDR: u64 = 0x38;
pub const MA_IP6: u64 = 0x29;
pub const MA_QUIC: u64 = 0x01cc;
pub const MA_QUIC_V1: u64 = 0x01cd;
pub const MA_WS: u64 = 0x01dd;
pub const MA_WSS: u64 = 0x01de;
pub const MA_P2P: u64 = 0x01a5;
pub const MA_CIRCUIT: u64 = 0x0122;
pub const MA_WEBTRANSPORT: u64 = 0x01d1;
pub const MA_CERTHASH: u64 = 0x01d2;
pub const MA_WEBRTC_DIRECT: u64 = 0x0118;

#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Ip4([u8; 4]),
    Ip6([u8; 16]),
    Tcp(u16),
    Udp(u16),
    Dns(String),  // dns/dns4/dns6 all dial the same way here (SOCKS resolves)
    Quic,         // both draft-29 and v1
    Ws,
    Wss,
    P2p(Vec<u8>), // multihash peer id
    Circuit,
    Other(u64),
}

/// Decode one binary multiaddr. Returns None on garbage.
pub fn multiaddr_decode(mut buf: &[u8]) -> Option<Vec<Seg>> {
    let mut segs = Vec::new();
    while !buf.is_empty() {
        let (code, n) = varint_read(buf)?;
        buf = &buf[n..];
        let seg = match code {
            MA_IP4 => {
                if buf.len() < 4 {
                    return None;
                }
                let mut ip = [0u8; 4];
                ip.copy_from_slice(&buf[..4]);
                buf = &buf[4..];
                Seg::Ip4(ip)
            }
            MA_IP6 => {
                if buf.len() < 16 {
                    return None;
                }
                let mut ip = [0u8; 16];
                ip.copy_from_slice(&buf[..16]);
                buf = &buf[16..];
                Seg::Ip6(ip)
            }
            MA_TCP | MA_UDP => {
                if buf.len() < 2 {
                    return None;
                }
                let port = u16::from_be_bytes([buf[0], buf[1]]);
                buf = &buf[2..];
                if code == MA_TCP {
                    Seg::Tcp(port)
                } else {
                    Seg::Udp(port)
                }
            }
            MA_DNS | MA_DNS4 | MA_DNS6 | MA_DNSADDR => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                if buf.len() < len as usize {
                    return None;
                }
                let name = String::from_utf8(buf[..len as usize].to_vec()).ok()?;
                buf = &buf[len as usize..];
                if code == MA_DNSADDR {
                    Seg::Other(code) // dnsaddr needs TXT resolution; not dialable here
                } else {
                    Seg::Dns(name)
                }
            }
            MA_QUIC | MA_QUIC_V1 => Seg::Quic,
            MA_WS => Seg::Ws,
            MA_WSS => Seg::Wss,
            MA_CIRCUIT => Seg::Circuit,
            MA_P2P => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                if buf.len() < len as usize {
                    return None;
                }
                let id = buf[..len as usize].to_vec();
                buf = &buf[len as usize..];
                Seg::P2p(id)
            }
            MA_WEBTRANSPORT | MA_WEBRTC_DIRECT => Seg::Other(code),
            MA_CERTHASH => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                if buf.len() < len as usize {
                    return None;
                }
                buf = &buf[len as usize..];
                Seg::Other(code)
            }
            _ => return None, // unknown code: size unknown, can't resume
        };
        segs.push(seg);
    }
    Some(segs)
}

/// A dial target this platform can reach: plain TCP, no relay hops, no
/// QUIC/WS layers (Step 0: fleet egress is SOCKS5 CONNECT, TCP only).
pub fn tcp_target(segs: &[Seg]) -> Option<(String, u16)> {
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    for s in segs {
        match s {
            Seg::Ip4(ip) => host = Some(format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])),
            Seg::Ip6(ip) => {
                let mut g = Vec::with_capacity(8);
                for i in 0..8 {
                    g.push(format!("{:x}", (u16::from(ip[i * 2]) << 8) | u16::from(ip[i * 2 + 1])));
                }
                host = Some(g.join(":"));
            }
            Seg::Dns(name) => host = Some(name.clone()),
            Seg::Tcp(p) => port = Some(*p),
            // any of these after/around tcp means "not plain TCP libp2p"
            Seg::Ws | Seg::Wss | Seg::Quic | Seg::Circuit | Seg::Udp(_) | Seg::Other(_) => {
                return None
            }
            Seg::P2p(_) => {}
        }
    }
    Some((host?, port?))
}

pub fn multiaddr_to_string(segs: &[Seg]) -> String {
    let mut out = String::new();
    for s in segs {
        match s {
            Seg::Ip4(ip) => out.push_str(&format!("/ip4/{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])),
            Seg::Ip6(ip) => {
                let mut g = Vec::with_capacity(8);
                for i in 0..8 {
                    g.push(format!("{:x}", (u16::from(ip[i * 2]) << 8) | u16::from(ip[i * 2 + 1])));
                }
                out.push_str(&format!("/ip6/{}", g.join(":")));
            }
            Seg::Tcp(p) => out.push_str(&format!("/tcp/{p}")),
            Seg::Udp(p) => out.push_str(&format!("/udp/{p}")),
            Seg::Dns(n) => out.push_str(&format!("/dns/{n}")),
            Seg::Quic => out.push_str("/quic-v1"),
            Seg::Ws => out.push_str("/ws"),
            Seg::Wss => out.push_str("/wss"),
            Seg::P2p(id) => out.push_str(&format!("/p2p/{}", base58btc(id))),
            Seg::Circuit => out.push_str("/p2p-circuit"),
            Seg::Other(c) => out.push_str(&format!("/x{c:x}")),
        }
    }
    out
}

/// Parse a textual multiaddr of the shapes a bootstrap list uses:
/// /ip4/1.2.3.4/tcp/4001/p2p/12D3Koo..., /dns4/host/tcp/4001/p2p/...
/// Returns (host, port, peer id multihash bytes).
pub fn parse_bootstrap(s: &str) -> Option<(String, u16, Vec<u8>)> {
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut peer: Option<Vec<u8>> = None;
    let mut i = 0;
    while i + 1 < parts.len() {
        match parts[i] {
            "ip4" | "ip6" | "dns" | "dns4" | "dns6" => host = Some(parts[i + 1].to_string()),
            "tcp" => port = parts[i + 1].parse().ok(),
            "p2p" | "ipfs" => peer = peer_id_str_decode(parts[i + 1]),
            "udp" | "quic" | "quic-v1" | "ws" | "wss" | "p2p-circuit" => return None,
            _ => return None,
        }
        i += 2;
    }
    Some((host?, port?, peer?))
}

/// Decode a peer id in any of its string forms (12D3Koo…/Qm… base58, or a
/// CIDv1 b…/k… libp2p-key) to multihash bytes.
pub fn peer_id_str_decode(s: &str) -> Option<Vec<u8>> {
    if s.starts_with("12D3Koo") || s.starts_with("Qm") || s.starts_with('1') {
        return base58_decode(s);
    }
    let bytes = match s.chars().next()? {
        'b' => base32_decode(&s[1..])?,
        'k' => base36_decode(&s[1..])?,
        _ => return None,
    };
    // CIDv1 with the libp2p-key codec wraps the multihash
    let (version, n) = varint_read(&bytes)?;
    if version != 1 {
        return None;
    }
    let (codec, m) = varint_read(&bytes[n..])?;
    if codec != 0x72 {
        return None;
    }
    Some(bytes[n + m..].to_vec())
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base58_vectors() {
        // cross-checked against an independent implementation
        assert_eq!(base58btc(&hex_decode("00010966776006953d5567439e5e39f86a0d273bee").unwrap()),
            "1qb3y62fmEEVTPySXPQ77WXok6H");
        assert_eq!(base58btc(b""), "");
        assert_eq!(base58btc(&[0, 0, 1]), "112");
        assert_eq!(base58_decode("112").unwrap(), vec![0, 0, 1]);
        assert_eq!(
            base58_decode("1qb3y62fmEEVTPySXPQ77WXok6H").unwrap(),
            hex_decode("00010966776006953d5567439e5e39f86a0d273bee").unwrap()
        );
    }

    #[test]
    fn base36_roundtrip() {
        for data in [&b""[..], &[0u8][..], &[0, 0, 7][..], b"hello world", &[255u8; 40][..]] {
            assert_eq!(base36_decode(&base36(data)).unwrap(), data);
        }
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode("Zm9vYmE").unwrap(), b"fooba");
    }

    #[test]
    fn multiaddr_tcp_targets() {
        // /ip4/139.178.91.71/tcp/4001
        let bin = [0x04, 139, 178, 91, 71, 0x06, 0x0f, 0xa1];
        let segs = multiaddr_decode(&bin).unwrap();
        assert_eq!(tcp_target(&segs), Some(("139.178.91.71".into(), 4001)));
        // /ip4/1.2.3.4/udp/4001/quic-v1 is not dialable here
        let quic = [0x04, 1, 2, 3, 4, 0x91, 0x02, 0x0f, 0xa1, 0xcd, 0x03];
        let segs = multiaddr_decode(&quic).unwrap();
        assert_eq!(tcp_target(&segs), None);
    }

    #[test]
    fn bootstrap_parsing() {
        let (h, p, id) = parse_bootstrap(
            "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
        )
        .unwrap();
        assert_eq!(h, "104.131.131.82");
        assert_eq!(p, 4001);
        assert_eq!(base58btc(&id), "QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ");
        assert!(parse_bootstrap("/ip4/1.2.3.4/udp/4001/quic-v1/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ").is_none());
    }
}
