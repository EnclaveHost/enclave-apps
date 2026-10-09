//! A byte buffer in fixed 8 MiB segments. A pushed pack is held whole while
//! it is indexed; growing one Vec to a gigabyte reallocates by doubling and
//! briefly needs twice the pack (more, at the last doubling), which a 4 GiB
//! guest cannot spare. Segments never move: memory is the pack plus at most
//! one segment of slack.

use std::borrow::Cow;

pub const SEG: usize = 8 << 20;

#[derive(Default)]
pub struct SegBuf {
    segs: Vec<Vec<u8>>,
    len: usize,
}

impl SegBuf {
    pub fn new() -> SegBuf {
        SegBuf::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn push(&mut self, mut d: &[u8]) {
        while !d.is_empty() {
            if self.segs.last().is_none_or(|s| s.len() == SEG) {
                // the first segment starts small (most pushes are small) and grows once
                let cap = if self.segs.is_empty() {
                    d.len().clamp(64 << 10, SEG)
                } else {
                    SEG
                };
                self.segs.push(Vec::with_capacity(cap));
            }
            let s = self.segs.last_mut().unwrap();
            let k = (SEG - s.len()).min(d.len());
            if s.len() + k > s.capacity() {
                s.reserve_exact(SEG - s.len());
            }
            s.extend_from_slice(&d[..k]);
            d = &d[k..];
            self.len += k;
        }
    }

    /// The contiguous run from `a` to the end of its segment (`a < len`).
    pub fn piece(&self, a: usize) -> &[u8] {
        let s = &self.segs[a / SEG];
        &s[a % SEG..]
    }

    /// Bytes [a, b): borrowed when they lie in one segment.
    pub fn slice(&self, a: usize, b: usize) -> Cow<'_, [u8]> {
        if a >= b {
            return Cow::Borrowed(&[]);
        }
        if a / SEG == (b - 1) / SEG {
            let o = a % SEG;
            return Cow::Borrowed(&self.segs[a / SEG][o..o + (b - a)]);
        }
        let mut v = Vec::with_capacity(b - a);
        self.each(a, b, |p| v.extend_from_slice(p));
        Cow::Owned(v)
    }

    /// Visit [a, b) as contiguous pieces.
    pub fn each(&self, a: usize, b: usize, mut f: impl FnMut(&[u8])) {
        let mut p = a;
        while p < b {
            let pc = self.piece(p);
            let k = pc.len().min(b - p);
            f(&pc[..k]);
            p += k;
        }
    }

    pub fn write_at(&mut self, at: usize, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            let p = at + i;
            self.segs[p / SEG][p % SEG] = b;
        }
    }

    pub fn truncate(&mut self, n: usize) {
        if n >= self.len {
            return;
        }
        let keep = n.div_ceil(SEG);
        self.segs.truncate(keep);
        if let Some(last) = self.segs.last_mut() {
            let tail = n - (keep - 1) * SEG;
            last.truncate(tail);
        }
        self.len = n;
    }

    #[cfg(test)]
    pub fn to_vec(&self) -> Vec<u8> {
        self.slice(0, self.len).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_behave_like_one_buffer() {
        let data: Vec<u8> = (0..(SEG * 2 + 1000)).map(|i| (i * 7 % 251) as u8).collect();
        let mut b = SegBuf::new();
        for ch in data.chunks(777_777) {
            b.push(ch);
        }
        assert_eq!(b.len(), data.len());
        for (a, e) in [
            (0, 10),
            (SEG - 5, SEG + 5),
            (SEG - 1, 2 * SEG + 3),
            (2 * SEG, data.len()),
        ] {
            assert_eq!(&*b.slice(a, e), &data[a..e]);
        }
        assert!(matches!(b.slice(10, 20), Cow::Borrowed(_)));
        b.write_at(SEG - 2, &[1, 2, 3, 4]);
        assert_eq!(&*b.slice(SEG - 2, SEG + 2), &[1, 2, 3, 4]);
        b.truncate(SEG + 1);
        assert_eq!(b.len(), SEG + 1);
        b.push(&[9, 9]);
        assert_eq!(&b.slice(SEG - 1, SEG + 3)[2..], &[9, 9]);
        b.truncate(SEG);
        assert_eq!(b.len(), SEG);
        b.push(&[5]);
        assert_eq!(&*b.slice(SEG, SEG + 1), &[5]);
    }
}
