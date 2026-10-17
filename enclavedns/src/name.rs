//! Domain names: uncompressed wire form, original case kept (DNSSEC signs
//! the case the signer saw, then canonicalizes), compared case-insensitively.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

#[derive(Clone)]
pub struct Name {
    /// Length-prefixed labels ending in the zero-length root label.
    wire: Vec<u8>,
}

pub const MAX_WIRE: usize = 255;

impl Name {
    pub fn root() -> Name {
        Name { wire: vec![0] }
    }

    pub fn from_wire_unchecked(wire: Vec<u8>) -> Name {
        Name { wire }
    }

    /// Presentation form: `www.example.com`, `www.example.com.`, `.`; `\.`
    /// and `\DDD` escapes. Relative names are taken as absolute.
    pub fn parse(s: &str) -> Result<Name, String> {
        let s = s.trim();
        if s.is_empty() || s == "." {
            return Ok(Name::root());
        }
        let b = s.as_bytes();
        let mut wire = Vec::with_capacity(b.len() + 2);
        let mut label: Vec<u8> = Vec::new();
        let mut i = 0;
        let mut ended_with_dot = false;
        let flush = |label: &mut Vec<u8>, wire: &mut Vec<u8>| -> Result<(), String> {
            if label.is_empty() {
                return Err(format!("empty label in {s:?}"));
            }
            if label.len() > 63 {
                return Err(format!("label longer than 63 octets in {s:?}"));
            }
            wire.push(label.len() as u8);
            wire.extend_from_slice(label);
            label.clear();
            Ok(())
        };
        while i < b.len() {
            ended_with_dot = false;
            match b[i] {
                b'.' => {
                    flush(&mut label, &mut wire)?;
                    ended_with_dot = true;
                    i += 1;
                }
                b'\\' => {
                    if i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
                        let v: u32 = std::str::from_utf8(&b[i + 1..i + 4]).unwrap().parse().unwrap();
                        if v > 255 {
                            return Err(format!("bad escape in {s:?}"));
                        }
                        label.push(v as u8);
                        i += 4;
                    } else if i + 1 < b.len() {
                        label.push(b[i + 1]);
                        i += 2;
                    } else {
                        return Err(format!("dangling escape in {s:?}"));
                    }
                }
                c => {
                    label.push(c);
                    i += 1;
                }
            }
        }
        if !ended_with_dot {
            flush(&mut label, &mut wire)?;
        }
        wire.push(0);
        if wire.len() > MAX_WIRE {
            return Err(format!("name longer than 255 octets: {s:?}"));
        }
        Ok(Name { wire })
    }

    /// Read a possibly-compressed name at `*pos` in `msg`. Pointers must go
    /// strictly backward, so a loop is impossible; the result is capped at
    /// 255 octets.
    pub fn read(msg: &[u8], pos: &mut usize) -> Result<Name, String> {
        let mut wire = Vec::with_capacity(32);
        let mut p = *pos;
        let mut jumped = false;
        let mut limit = p; // a pointer must land before this
        loop {
            let len = *msg.get(p).ok_or("name runs past the message")? as usize;
            match len & 0xC0 {
                0x00 => {
                    if len == 0 {
                        wire.push(0);
                        if !jumped {
                            *pos = p + 1;
                        }
                        break;
                    }
                    let lab = msg.get(p + 1..p + 1 + len).ok_or("label runs past the message")?;
                    wire.push(len as u8);
                    wire.extend_from_slice(lab);
                    if wire.len() + 1 > MAX_WIRE {
                        return Err("name longer than 255 octets".into());
                    }
                    p += 1 + len;
                }
                0xC0 => {
                    let lo = *msg.get(p + 1).ok_or("pointer runs past the message")? as usize;
                    let target = ((len & 0x3F) << 8) | lo;
                    if target >= limit {
                        return Err("compression pointer does not point backward".into());
                    }
                    if !jumped {
                        *pos = p + 2;
                        jumped = true;
                    }
                    limit = target;
                    p = target;
                }
                _ => return Err("unsupported label type".into()),
            }
        }
        Ok(Name { wire })
    }

    pub fn wire(&self) -> &[u8] {
        &self.wire
    }

    pub fn is_root(&self) -> bool {
        self.wire.len() == 1
    }

    /// Labels left to right, root excluded.
    pub fn labels(&self) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut i = 0;
        while self.wire[i] != 0 {
            let l = self.wire[i] as usize;
            out.push(&self.wire[i + 1..i + 1 + l]);
            i += 1 + l;
        }
        out
    }

    pub fn label_count(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while self.wire[i] != 0 {
            n += 1;
            i += 1 + self.wire[i] as usize;
        }
        n
    }

    pub fn first_label(&self) -> &[u8] {
        let l = self.wire[0] as usize;
        &self.wire[1..1 + l]
    }

    pub fn is_wildcard(&self) -> bool {
        self.first_label() == b"*"
    }

    pub fn parent(&self) -> Option<Name> {
        if self.is_root() {
            return None;
        }
        let l = self.wire[0] as usize;
        Some(Name { wire: self.wire[1 + l..].to_vec() })
    }

    /// The rightmost `n` labels (n = 0 is the root).
    pub fn suffix(&self, n: usize) -> Name {
        let total = self.label_count();
        let mut cur = self.clone();
        for _ in n..total {
            cur = cur.parent().unwrap();
        }
        cur
    }

    pub fn prepend(&self, label: &[u8]) -> Result<Name, String> {
        if label.is_empty() || label.len() > 63 {
            return Err("bad label".into());
        }
        let mut wire = Vec::with_capacity(self.wire.len() + label.len() + 1);
        wire.push(label.len() as u8);
        wire.extend_from_slice(label);
        wire.extend_from_slice(&self.wire);
        if wire.len() > MAX_WIRE {
            return Err("name too long".into());
        }
        Ok(Name { wire })
    }

    /// Replace the `old_suffix` this name ends in with `new_suffix` (DNAME).
    pub fn replace_suffix(&self, old_suffix: &Name, new_suffix: &Name) -> Result<Name, String> {
        if !self.is_subdomain_of(old_suffix) {
            return Err("not under the DNAME owner".into());
        }
        let keep = self.wire.len() - old_suffix.wire.len();
        let mut wire = self.wire[..keep].to_vec();
        wire.extend_from_slice(&new_suffix.wire);
        if wire.len() > MAX_WIRE {
            return Err("DNAME substitution overflows".into());
        }
        Ok(Name { wire })
    }

    pub fn to_lowercase(&self) -> Name {
        Name { wire: self.wire.to_ascii_lowercase() }
    }

    /// `self` equals `other` or lies beneath it.
    pub fn is_subdomain_of(&self, other: &Name) -> bool {
        let (a, b) = (self.label_count(), other.label_count());
        if a < b {
            return false;
        }
        self.suffix(b) == *other
    }

    /// The deepest name both lie under.
    pub fn common_ancestor(&self, other: &Name) -> Name {
        let (a, b) = (self.labels(), other.labels());
        let mut n = 0;
        while n < a.len() && n < b.len() && a[a.len() - 1 - n].eq_ignore_ascii_case(b[b.len() - 1 - n]) {
            n += 1;
        }
        self.suffix(n)
    }

    /// RFC 4034 §6.1: compare label by label from the right, each as
    /// lowercase octets, a shorter label sorting first on a common prefix.
    pub fn canonical_cmp(&self, other: &Name) -> Ordering {
        let (a, b) = (self.labels(), other.labels());
        let (mut i, mut j) = (a.len(), b.len());
        while i > 0 && j > 0 {
            i -= 1;
            j -= 1;
            let (x, y) = (a[i].to_ascii_lowercase(), b[j].to_ascii_lowercase());
            match x.cmp(&y) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        (a.len()).cmp(&b.len())
    }

    pub fn lower_string(&self) -> String {
        self.to_string().to_ascii_lowercase()
    }

    /// Presentation form without the trailing dot (the root is ".").
    pub fn to_text(&self) -> String {
        let s = self.to_string();
        if s == "." {
            s
        } else {
            s.trim_end_matches('.').to_string()
        }
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Name) -> bool {
        self.wire.len() == other.wire.len() && self.wire.eq_ignore_ascii_case(&other.wire)
    }
}
impl Eq for Name {}

impl Hash for Name {
    fn hash<H: Hasher>(&self, h: &mut H) {
        for b in &self.wire {
            h.write_u8(b.to_ascii_lowercase());
        }
    }
}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Name) -> Option<Ordering> {
        Some(self.canonical_cmp(other))
    }
}
impl Ord for Name {
    fn cmp(&self, other: &Name) -> Ordering {
        self.canonical_cmp(other)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_root() {
            return f.write_str(".");
        }
        for l in self.labels() {
            for &c in l {
                match c {
                    b'.' | b'\\' | b'"' | b'(' | b')' | b';' | b'@' | b'$' => write!(f, "\\{}", c as char)?,
                    0x21..=0x7E => write!(f, "{}", c as char)?,
                    _ => write!(f, "\\{:03}", c)?,
                }
            }
            f.write_str(".")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_print() {
        let n = Name::parse("WWW.Example.com").unwrap();
        assert_eq!(n.to_string(), "WWW.Example.com.");
        assert_eq!(n, Name::parse("www.example.COM.").unwrap());
        assert_eq!(n.label_count(), 3);
        assert_eq!(Name::parse(".").unwrap(), Name::root());
        assert_eq!(Name::parse("a\\.b.c").unwrap().label_count(), 2);
        assert_eq!(Name::parse("\\000.x").unwrap().first_label(), &[0u8]);
        assert!(Name::parse("a..b").is_err());
    }

    #[test]
    fn canonical_order_rfc4034() {
        // the example ordering in RFC 4034 §6.1
        let order = [
            "example", "a.example", "yljkjljk.a.example", "Z.a.example", "zABC.a.EXAMPLE",
            "z.example", "\\001.z.example", "*.z.example", "\\200.z.example",
        ];
        let names: Vec<Name> = order.iter().map(|s| Name::parse(s).unwrap()).collect();
        for w in names.windows(2) {
            assert_eq!(w[0].canonical_cmp(&w[1]), Ordering::Less, "{} < {}", w[0], w[1]);
        }
    }

    #[test]
    fn compression_pointers() {
        // header-less buffer: "example.com" at 0, then "www" + pointer to 0
        let mut m = vec![7];
        m.extend_from_slice(b"example");
        m.push(3);
        m.extend_from_slice(b"com");
        m.push(0);
        let at = m.len();
        m.push(3);
        m.extend_from_slice(b"www");
        m.extend_from_slice(&[0xC0, 0]);
        let mut p = at;
        let n = Name::read(&m, &mut p).unwrap();
        assert_eq!(n.to_string(), "www.example.com.");
        assert_eq!(p, m.len());
        // a pointer to itself is refused
        let mut bad = vec![0xC0, 0];
        let mut p = 0;
        assert!(Name::read(&bad, &mut p).is_err());
        bad = vec![1, b'a', 0xC0, 2];
        let mut p = 0;
        assert!(Name::read(&bad, &mut p).is_err());
    }

    #[test]
    fn relations() {
        let a = Name::parse("a.b.example.com").unwrap();
        let z = Name::parse("example.com").unwrap();
        assert!(a.is_subdomain_of(&z));
        assert!(!z.is_subdomain_of(&a));
        assert_eq!(a.suffix(2), z);
        assert_eq!(a.common_ancestor(&Name::parse("x.example.com").unwrap()), z);
        let d = a
            .replace_suffix(&Name::parse("b.example.com").unwrap(), &Name::parse("other.net").unwrap())
            .unwrap();
        assert_eq!(d.to_string(), "a.other.net.");
    }
}
