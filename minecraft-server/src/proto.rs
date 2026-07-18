//! Wire primitives for Minecraft protocol 47 (1.8.x), uncompressed and
//! unencrypted (we never send Set Compression and run offline-mode, so every
//! frame is `varint length | varint packet-id | body`).

/// A cursor over one received frame. Every read is bounds-checked; any
/// malformed packet surfaces as Err(()) and the connection is dropped.
pub struct R<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        R { b, p: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.b.len() - self.p
    }

    pub fn u8(&mut self) -> Result<u8, ()> {
        let v = *self.b.get(self.p).ok_or(())?;
        self.p += 1;
        Ok(v)
    }

    pub fn i8(&mut self) -> Result<i8, ()> {
        Ok(self.u8()? as i8)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), ()> {
        if self.remaining() < n {
            return Err(());
        }
        self.p += n;
        Ok(())
    }

    fn chunk(&mut self, n: usize) -> Result<&'a [u8], ()> {
        if self.remaining() < n {
            return Err(());
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    pub fn u16(&mut self) -> Result<u16, ()> {
        let c = self.chunk(2)?;
        Ok(u16::from_be_bytes([c[0], c[1]]))
    }

    pub fn i16(&mut self) -> Result<i16, ()> {
        Ok(self.u16()? as i16)
    }

    pub fn i32(&mut self) -> Result<i32, ()> {
        let c = self.chunk(4)?;
        Ok(i32::from_be_bytes([c[0], c[1], c[2], c[3]]))
    }

    pub fn i64(&mut self) -> Result<i64, ()> {
        let c = self.chunk(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(c);
        Ok(i64::from_be_bytes(a))
    }

    pub fn u64(&mut self) -> Result<u64, ()> {
        Ok(self.i64()? as u64)
    }

    pub fn f32(&mut self) -> Result<f32, ()> {
        Ok(f32::from_bits(self.i32()? as u32))
    }

    pub fn f64(&mut self) -> Result<f64, ()> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    pub fn bool(&mut self) -> Result<bool, ()> {
        Ok(self.u8()? != 0)
    }

    pub fn varint(&mut self) -> Result<i32, ()> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7F) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i32);
            }
        }
        Err(())
    }

    pub fn string(&mut self, max: usize) -> Result<String, ()> {
        let n = self.varint()?;
        if n < 0 || n as usize > max * 4 {
            return Err(());
        }
        let s = std::str::from_utf8(self.chunk(n as usize)?).map_err(|_| ())?;
        if s.chars().count() > max {
            return Err(());
        }
        Ok(s.to_string())
    }
}

/// Try to split one frame off `buf`. Returns (frame_start, frame_end) into the
/// buffer — i.e. the packet body without the length prefix — plus the total
/// bytes consumed. None = incomplete; Err = malformed/oversized.
pub fn split_frame(buf: &[u8], max_len: usize) -> Result<Option<(usize, usize)>, ()> {
    let mut len: u32 = 0;
    for i in 0..5 {
        let Some(&b) = buf.get(i) else {
            return Ok(None); // length prefix itself incomplete
        };
        len |= ((b & 0x7F) as u32) << (7 * i);
        if b & 0x80 == 0 {
            let len = len as usize;
            if len == 0 || len > max_len {
                return Err(());
            }
            let start = i + 1;
            if buf.len() < start + len {
                return Ok(None);
            }
            return Ok(Some((start, start + len)));
        }
    }
    Err(())
}

/// Outgoing packet builder: body (starting with the packet-id varint) is
/// accumulated, then framed with a length prefix by `frame()`.
pub struct P {
    pub b: Vec<u8>,
}

impl P {
    pub fn new(id: i32) -> Self {
        let mut p = P { b: Vec::with_capacity(64) };
        p.varint(id);
        p
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.b.push(v);
        self
    }

    pub fn i8(&mut self, v: i8) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.u16(v as u16)
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.i64(v as i64)
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.i32(v.to_bits() as i32)
    }

    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.i64(v.to_bits() as i64)
    }

    pub fn varint(&mut self, v: i32) -> &mut Self {
        let mut v = v as u32;
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.b.push(b);
                break;
            }
            self.b.push(b | 0x80);
        }
        self
    }

    pub fn string(&mut self, s: &str) -> &mut Self {
        self.varint(s.len() as i32);
        self.b.extend_from_slice(s.as_bytes());
        self
    }

    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.b.extend_from_slice(bytes);
        self
    }

    /// Angle in degrees → 1/256th-turn byte.
    pub fn angle(&mut self, deg: f32) -> &mut Self {
        self.u8(((deg.rem_euclid(360.0) / 360.0) * 256.0) as i64 as u8)
    }

    /// Block coordinates → 1.8 packed Position (x:26 y:12 z:26 bits).
    pub fn position(&mut self, x: i32, y: i32, z: i32) -> &mut Self {
        self.u64(pack_position(x, y, z))
    }

    /// Entity coordinate → 1.8 fixed-point (1/32 block) int.
    pub fn fixed(&mut self, v: f64) -> &mut Self {
        self.i32((v * 32.0).floor() as i32)
    }

    /// Prefix with the length varint, producing the on-wire frame.
    pub fn frame(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.b.len() + 5);
        let mut v = self.b.len() as u32;
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend_from_slice(&self.b);
        out
    }
}

pub fn pack_position(x: i32, y: i32, z: i32) -> u64 {
    ((x as u64 & 0x3FF_FFFF) << 38) | ((y as u64 & 0xFFF) << 26) | (z as u64 & 0x3FF_FFFF)
}

/// Unpack a 1.8 Position with sign extension.
pub fn unpack_position(v: u64) -> (i32, i32, i32) {
    let x = (v >> 38) as i32 & 0x3FF_FFFF;
    let y = (v >> 26) as i32 & 0xFFF;
    let z = v as i32 & 0x3FF_FFFF;
    let sx = if x >= 1 << 25 { x - (1 << 26) } else { x };
    let sy = if y >= 1 << 11 { y - (1 << 12) } else { y };
    let sz = if z >= 1 << 25 { z - (1 << 26) } else { z };
    (sx, sy, sz)
}

/// 1.8 Slot data: item id, or None for the empty slot. NBT payload (if any)
/// is validated/skipped. Returns (item_id, count, damage).
pub fn read_slot(r: &mut R) -> Result<Option<(i16, u8, i16)>, ()> {
    let id = r.i16()?;
    if id == -1 {
        return Ok(None);
    }
    let count = r.u8()?;
    let damage = r.i16()?;
    let tag = r.u8()?;
    if tag != 0 {
        // A named tag: name follows the type byte, then the payload.
        let n = r.u16()? as usize;
        r.skip(n)?;
        skip_nbt_payload(r, tag, 0)?;
    }
    Ok(Some((id, count, damage)))
}

fn skip_nbt_payload(r: &mut R, tag: u8, depth: u8) -> Result<(), ()> {
    if depth > 24 {
        return Err(());
    }
    match tag {
        1 => r.skip(1),
        2 => r.skip(2),
        3 | 5 => r.skip(4),
        4 | 6 => r.skip(8),
        7 => {
            let n = r.i32()?;
            if n < 0 {
                return Err(());
            }
            r.skip(n as usize)
        }
        8 => {
            let n = r.u16()? as usize;
            r.skip(n)
        }
        9 => {
            let t = r.u8()?;
            let n = r.i32()?;
            if n < 0 {
                return Err(());
            }
            for _ in 0..n {
                skip_nbt_payload(r, t, depth + 1)?;
            }
            Ok(())
        }
        10 => {
            loop {
                let t = r.u8()?;
                if t == 0 {
                    return Ok(());
                }
                let n = r.u16()? as usize;
                r.skip(n)?;
                skip_nbt_payload(r, t, depth + 1)?;
            }
        }
        11 => {
            let n = r.i32()?;
            if n < 0 {
                return Err(());
            }
            r.skip(n as usize * 4)
        }
        12 => {
            let n = r.i32()?;
            if n < 0 {
                return Err(());
            }
            r.skip(n as usize * 8)
        }
        _ => Err(()),
    }
}

/// Escape arbitrary text for embedding in a JSON string literal.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Deterministic offline UUID from a player name (16 bytes, version/variant
/// bits set). Vanilla uses md5("OfflinePlayer:"+name); any stable UUID works
/// for an offline server — the client only needs list/spawn consistency.
pub fn offline_uuid(name: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut h1: u64 = 0xcbf29ce484222325;
    let mut h2: u64 = 0x9e3779b97f4a7c15;
    for &b in name.as_bytes() {
        h1 = (h1 ^ b as u64).wrapping_mul(0x100000001b3);
        h2 = (h2 ^ (b as u64).rotate_left(17)).wrapping_mul(0xff51afd7ed558ccd);
        h2 ^= h2 >> 29;
    }
    out[..8].copy_from_slice(&h1.to_be_bytes());
    out[8..].copy_from_slice(&h2.to_be_bytes());
    out[6] = (out[6] & 0x0F) | 0x30; // version 3
    out[8] = (out[8] & 0x3F) | 0x80; // RFC 4122 variant
    out
}

pub fn uuid_string(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{:02x}", b)).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}
