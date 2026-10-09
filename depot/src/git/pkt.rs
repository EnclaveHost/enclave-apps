//! pkt-line framing: four hex digits of length (including themselves), then
//! the payload; `0000` flush, `0001` delimiter, `0002` response-end.

pub const MAX_PKT: usize = 65520;
/// Largest side-band-64k payload: the pkt maximum less length and band byte.
pub const MAX_BAND: usize = MAX_PKT - 5;

#[derive(Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    End,
}

impl<'a> Pkt<'a> {
    /// The payload as text without its trailing newline (git's CHOMP).
    pub fn text(&self) -> Option<&'a str> {
        match self {
            Pkt::Data(d) => {
                let d = d.strip_suffix(b"\n").unwrap_or(d);
                std::str::from_utf8(d).ok()
            }
            _ => None,
        }
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    /// `Ok(None)` at a clean end of input.
    pub fn next(&mut self) -> Result<Option<Pkt<'a>>, String> {
        if self.pos == self.buf.len() {
            return Ok(None);
        }
        match parse(&self.buf[self.pos..])? {
            Some((pkt, n)) => {
                self.pos += n;
                Ok(Some(pkt))
            }
            None => Err("truncated pkt-line".into()),
        }
    }
}

/// One pkt from the front of `b`: `Ok(None)` when more bytes are needed.
pub fn parse(b: &[u8]) -> Result<Option<(Pkt<'_>, usize)>, String> {
    if b.len() < 4 {
        return Ok(None);
    }
    let len = std::str::from_utf8(&b[..4])
        .ok()
        .and_then(|s| usize::from_str_radix(s, 16).ok())
        .ok_or("bad pkt-line length")?;
    match len {
        0 => Ok(Some((Pkt::Flush, 4))),
        1 => Ok(Some((Pkt::Delim, 4))),
        2 => Ok(Some((Pkt::End, 4))),
        3 => Err("bad pkt-line length".into()),
        n if n > MAX_PKT => Err("pkt-line too long".into()),
        n if b.len() < n => Ok(None),
        n => Ok(Some((Pkt::Data(&b[4..n]), n))),
    }
}

pub fn data(out: &mut Vec<u8>, payload: &[u8]) {
    debug_assert!(payload.len() + 4 <= MAX_PKT);
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
}

pub fn line(out: &mut Vec<u8>, s: &str) {
    data(out, s.as_bytes());
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub fn delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

/// Wrap `payload` in side-band packets on `band` (1 data, 2 progress,
/// 3 fatal error); `max` is the per-packet payload ceiling negotiated
/// (side-band-64k: MAX_BAND; plain side-band: 995).
pub fn band(out: &mut Vec<u8>, band: u8, payload: &[u8], max: usize) {
    for chunk in payload.chunks(max.max(1)) {
        out.extend_from_slice(format!("{:04x}", chunk.len() + 5).as_bytes());
        out.push(band);
        out.extend_from_slice(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut b = Vec::new();
        line(&mut b, "want abc\n");
        delim(&mut b);
        flush(&mut b);
        assert_eq!(&b, b"000dwant abc\n00010000");
        let mut r = Reader::new(&b);
        assert_eq!(r.next().unwrap().unwrap().text(), Some("want abc"));
        assert_eq!(r.next().unwrap(), Some(Pkt::Delim));
        assert_eq!(r.next().unwrap(), Some(Pkt::Flush));
        assert_eq!(r.next().unwrap(), None);
        assert!(Reader::new(b"00").next().is_err());
        assert!(parse(b"0005").unwrap().is_none());
        assert!(parse(b"zzzz").is_err());
    }

    #[test]
    fn bands_split() {
        let mut b = Vec::new();
        band(&mut b, 1, &[7u8; 2000], 995);
        let mut r = Reader::new(&b);
        let mut total = 0;
        while let Some(Pkt::Data(d)) = r.next().unwrap() {
            assert_eq!(d[0], 1);
            assert!(d.len() <= 996);
            total += d.len() - 1;
        }
        assert_eq!(total, 2000);
    }
}
