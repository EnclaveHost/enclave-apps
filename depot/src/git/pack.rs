//! The pack format: `PACK`, version, object count; entries of a type+size
//! varint header (plus the base for deltas) and a zlib stream; a SHA-1
//! trailer over everything before it.

use super::Oid;
use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

pub const OFS_DELTA: u8 = 6;
pub const REF_DELTA: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    None,
    /// absolute pack offset of the base entry
    Ofs(u64),
    Ref(Oid),
}

#[derive(Clone, Copy, Debug)]
pub struct EntryHeader {
    /// 1..=4 (a whole object) or OFS_DELTA / REF_DELTA
    pub kind: u8,
    /// inflated length of the entry's data (for a delta: the delta's length)
    pub size: u64,
    /// bytes before the zlib stream
    pub header_len: usize,
    pub base: Base,
}

/// Parse the entry at `offset` from `b` (which starts there). `Ok(None)`:
/// more bytes needed.
pub fn parse_header(b: &[u8], offset: u64) -> Result<Option<EntryHeader>, String> {
    let mut p = 0usize;
    let Some(&c) = b.first() else { return Ok(None) };
    p += 1;
    let kind = (c >> 4) & 7;
    let mut size = (c & 15) as u64;
    let mut shift = 4;
    let mut c = c;
    while c & 0x80 != 0 {
        let Some(&n) = b.get(p) else { return Ok(None) };
        p += 1;
        if shift > 60 {
            return Err("pack entry size overflow".into());
        }
        size |= ((n & 0x7f) as u64) << shift;
        shift += 7;
        c = n;
    }
    let base = match kind {
        1..=4 => Base::None,
        OFS_DELTA => {
            let Some(&n) = b.get(p) else { return Ok(None) };
            p += 1;
            let mut d = (n & 0x7f) as u64;
            let mut n = n;
            while n & 0x80 != 0 {
                let Some(&m) = b.get(p) else { return Ok(None) };
                p += 1;
                if d >= (1u64 << 56) {
                    return Err("pack delta offset overflow".into());
                }
                d = ((d + 1) << 7) | (m & 0x7f) as u64;
                n = m;
            }
            if d == 0 || d > offset {
                return Err("pack delta base offset out of range".into());
            }
            Base::Ofs(offset - d)
        }
        REF_DELTA => {
            if b.len() < p + 20 {
                return Ok(None);
            }
            let o = Oid::from_slice(&b[p..p + 20]).unwrap_or_default();
            p += 20;
            Base::Ref(o)
        }
        _ => return Err(format!("bad pack entry type {kind}")),
    };
    Ok(Some(EntryHeader {
        kind,
        size,
        header_len: p,
        base,
    }))
}

pub fn encode_header(kind: u8, size: u64, out: &mut Vec<u8>) {
    let mut c = (kind << 4) | (size & 15) as u8;
    let mut s = size >> 4;
    while s != 0 {
        out.push(c | 0x80);
        c = (s & 0x7f) as u8;
        s >>= 7;
    }
    out.push(c);
}

pub fn encode_ofs(distance: u64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 10];
    let mut i = buf.len() - 1;
    let mut d = distance;
    buf[i] = (d & 0x7f) as u8;
    d >>= 7;
    while d != 0 {
        d -= 1;
        i -= 1;
        buf[i] = 0x80 | (d & 0x7f) as u8;
        d >>= 7;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Inflate one zlib stream from the front of `data`, which must hold all
/// of it. Returns the content and the compressed length.
pub fn inflate_at(data: &[u8], expect: u64) -> Result<(Vec<u8>, usize), String> {
    if expect > (u32::MAX as u64) * 4 {
        return Err("object too large".into());
    }
    let mut st = InflateState::new_boxed(DataFormat::Zlib);
    let mut out = vec![0u8; expect as usize];
    let mut inpos = 0usize;
    let mut outpos = 0usize;
    let mut spare = [0u8; 64];
    loop {
        let dst: &mut [u8] = if outpos < out.len() {
            &mut out[outpos..]
        } else {
            &mut spare
        };
        let r = inflate(&mut st, &data[inpos..], dst, MZFlush::None);
        inpos += r.bytes_consumed;
        if outpos < out.len() {
            outpos += r.bytes_written;
        } else if r.bytes_written > 0 {
            return Err("pack entry inflates past its declared size".into());
        }
        match r.status {
            Ok(MZStatus::StreamEnd) => break,
            Ok(_) if r.bytes_consumed == 0 && r.bytes_written == 0 => {
                return Err("truncated zlib stream".into())
            }
            Ok(_) => {}
            Err(_) => return Err("corrupt zlib stream".into()),
        }
    }
    if outpos as u64 != expect {
        return Err("pack entry size mismatch".into());
    }
    Ok((out, inpos))
}

pub fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        for (kind, size) in [
            (1u8, 0u64),
            (3, 15),
            (3, 16),
            (2, 1 << 20),
            (4, u32::MAX as u64 * 3),
        ] {
            let mut b = Vec::new();
            encode_header(kind, size, &mut b);
            let h = parse_header(&b, 100).unwrap().unwrap();
            assert_eq!((h.kind, h.size, h.header_len), (kind, size, b.len()));
            assert!(parse_header(&b[..b.len() - 1], 100).unwrap().is_none() || b.len() == 1);
        }
    }

    #[test]
    fn ofs_roundtrip() {
        for d in [1u64, 127, 128, 16511, 16512, 1 << 30, 123_456_789] {
            let mut b = Vec::new();
            encode_header(OFS_DELTA, 10, &mut b);
            encode_ofs(d, &mut b);
            let h = parse_header(&b, d + 5).unwrap().unwrap();
            assert_eq!(h.base, Base::Ofs(5), "distance {d}");
            assert!(parse_header(&b, d - 1).is_err());
        }
    }

    #[test]
    fn inflate_exact() {
        let z = deflate(b"some content");
        let mut buf = z.clone();
        buf.extend_from_slice(b"trailing");
        let (c, n) = inflate_at(&buf, 12).unwrap();
        assert_eq!((c.as_slice(), n), (&b"some content"[..], z.len()));
        assert!(inflate_at(&buf, 11).is_err());
        assert!(inflate_at(&buf, 13).is_err());
        assert!(inflate_at(&z[..z.len() - 3], 12).is_err());
    }
}
