//! yamux framing — the stream multiplexer libp2p runs inside the Noise
//! channel. Frames only; session/stream state lives in p2p.rs where the
//! event loop can see it. Spec: hashicorp/yamux SPEC.md, /yamux/1.0.0.

#![allow(dead_code)]

pub const HEADER: usize = 12;

pub const TYPE_DATA: u8 = 0;
pub const TYPE_WINDOW: u8 = 1;
pub const TYPE_PING: u8 = 2;
pub const TYPE_GOAWAY: u8 = 3;

pub const FLAG_SYN: u16 = 1;
pub const FLAG_ACK: u16 = 2;
pub const FLAG_FIN: u16 = 4;
pub const FLAG_RST: u16 = 8;

/// Both sides start every stream with this much receive window.
pub const INITIAL_WINDOW: u32 = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub typ: u8,
    pub flags: u16,
    pub stream_id: u32,
    /// Data length for TYPE_DATA; delta for TYPE_WINDOW; opaque for
    /// TYPE_PING; error code for TYPE_GOAWAY.
    pub length: u32,
}

impl Frame {
    pub fn encode(&self) -> [u8; HEADER] {
        let mut h = [0u8; HEADER];
        h[0] = 0; // version
        h[1] = self.typ;
        h[2..4].copy_from_slice(&self.flags.to_be_bytes());
        h[4..8].copy_from_slice(&self.stream_id.to_be_bytes());
        h[8..12].copy_from_slice(&self.length.to_be_bytes());
        h
    }

    /// Decode one header. The caller checks that TYPE_DATA frames have
    /// `length` more bytes buffered before consuming.
    pub fn decode(h: &[u8]) -> Result<Frame, String> {
        if h.len() < HEADER {
            return Err("yamux: short header".into());
        }
        if h[0] != 0 {
            return Err(format!("yamux: unknown version {}", h[0]));
        }
        let typ = h[1];
        if typ > TYPE_GOAWAY {
            return Err(format!("yamux: unknown frame type {typ}"));
        }
        Ok(Frame {
            typ,
            flags: u16::from_be_bytes([h[2], h[3]]),
            stream_id: u32::from_be_bytes([h[4], h[5], h[6], h[7]]),
            length: u32::from_be_bytes([h[8], h[9], h[10], h[11]]),
        })
    }
}

pub fn data(stream_id: u32, flags: u16, payload_len: u32) -> Frame {
    Frame { typ: TYPE_DATA, flags, stream_id, length: payload_len }
}
pub fn window_update(stream_id: u32, flags: u16, delta: u32) -> Frame {
    Frame { typ: TYPE_WINDOW, flags, stream_id, length: delta }
}
pub fn ping(flags: u16, opaque: u32) -> Frame {
    Frame { typ: TYPE_PING, flags, stream_id: 0, length: opaque }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for f in [
            data(1, FLAG_SYN, 512),
            window_update(2, FLAG_ACK, 65536),
            ping(FLAG_SYN, 0xdeadbeef),
            Frame { typ: TYPE_GOAWAY, flags: 0, stream_id: 0, length: 0 },
        ] {
            assert_eq!(Frame::decode(&f.encode()).unwrap(), f);
        }
        assert!(Frame::decode(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        assert!(Frame::decode(&[0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }
}
