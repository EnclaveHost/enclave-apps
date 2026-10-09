//! git's delta format: two size varints (base, result), then copy
//! instructions (MSB set: offset/size bytes from the base) and insert
//! instructions (1..=127 literal bytes).

fn varint(d: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *d.get(*pos).ok_or("truncated delta header")?;
        *pos += 1;
        if shift > 63 {
            return Err("delta size overflow".into());
        }
        v |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
}

/// (base size, result size) from the front of a delta.
#[cfg(test)]
pub fn sizes(d: &[u8]) -> Result<(u64, u64), String> {
    let mut p = 0;
    Ok((varint(d, &mut p)?, varint(d, &mut p)?))
}

pub fn apply(base: &[u8], d: &[u8], max_result: u64) -> Result<Vec<u8>, String> {
    let mut p = 0;
    let bsize = varint(d, &mut p)?;
    let rsize = varint(d, &mut p)?;
    if bsize != base.len() as u64 {
        return Err("delta base size mismatch".into());
    }
    if rsize > max_result {
        return Err("delta result exceeds the object size limit".into());
    }
    let mut out = Vec::with_capacity(rsize as usize);
    while p < d.len() {
        let op = d[p];
        p += 1;
        if op & 0x80 != 0 {
            let mut off = 0u64;
            let mut size = 0u64;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off |= (*d.get(p).ok_or("truncated delta copy")? as u64) << (8 * i);
                    p += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    size |= (*d.get(p).ok_or("truncated delta copy")? as u64) << (8 * i);
                    p += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = off.checked_add(size).ok_or("delta copy overflow")?;
            if end > base.len() as u64 || out.len() as u64 + size > rsize {
                return Err("delta copy out of range".into());
            }
            out.extend_from_slice(&base[off as usize..end as usize]);
        } else if op != 0 {
            let n = op as usize;
            if p + n > d.len() || out.len() + n > rsize as usize {
                return Err("delta insert out of range".into());
            }
            out.extend_from_slice(&d[p..p + n]);
            p += n;
        } else {
            return Err("delta opcode 0 is reserved".into());
        }
    }
    if out.len() as u64 != rsize {
        return Err("delta result size mismatch".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(mut v: u64, out: &mut Vec<u8>) {
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

    #[test]
    fn copy_and_insert() {
        let base = b"hello, wonderful world";
        let mut d = Vec::new();
        enc(base.len() as u64, &mut d);
        enc(18, &mut d);
        d.extend_from_slice(&[0x80 | 0x10, 5]); // copy off 0 len 5 -> "hello"
        d.extend_from_slice(&[7]);
        d.extend_from_slice(b" brave ");
        d.extend_from_slice(&[0x80 | 0x01 | 0x10, 17, 5]); // copy off 17 len 5 -> "world"
        d.push(1);
        d.push(b'!');
        assert_eq!(apply(base, &d, 1 << 20).unwrap(), b"hello brave world!");
        assert_eq!(sizes(&d).unwrap(), (22, 18));
        // wrong base, oversize copy, reserved op
        assert!(apply(b"short", &d, 1 << 20).is_err());
        let mut bad = d.clone();
        bad[2] = 0x80 | 0x10;
        bad[3] = 200;
        assert!(apply(base, &bad, 1 << 20).is_err());
        let mut z = Vec::new();
        enc(22, &mut z);
        enc(1, &mut z);
        z.push(0);
        assert!(apply(base, &z, 1 << 20).is_err());
        assert!(apply(base, &d, 10).is_err());
    }
}
