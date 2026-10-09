//! The git object model, as much of it as a server needs: object ids, the
//! four object types, pkt-lines, the pack and delta codecs, the per-pack
//! index this app stores, the in-memory repository graph, and the three
//! protocol handlers (upload-pack v2 and v0, receive-pack).

pub mod delta;
pub mod ingest;
pub mod object;
pub mod pack;
pub mod packgen;
pub mod pkt;
pub mod receive;
pub mod repo;
pub mod segbuf;
pub mod upload;
pub mod walk;

use std::fmt;

/// A SHA-1 object id. Only object-format=sha1 is served.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Oid(pub [u8; 20]);

pub const ZERO: Oid = Oid([0; 20]);

impl Oid {
    pub fn from_hex(s: &str) -> Option<Oid> {
        let b = s.as_bytes();
        if b.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
        }
        Some(Oid(out))
    }
    pub fn from_slice(b: &[u8]) -> Option<Oid> {
        let a: [u8; 20] = b.try_into().ok()?;
        Some(Oid(a))
    }
    pub fn hex(&self) -> String {
        hex(&self.0)
    }
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}
impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    (0..b.len() / 2)
        .map(|i| Some((nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?))
        .collect()
}

/// The four object types, numbered as in the pack format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        match v {
            1 => Some(Kind::Commit),
            2 => Some(Kind::Tree),
            3 => Some(Kind::Blob),
            4 => Some(Kind::Tag),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }
    pub fn from_name(s: &str) -> Option<Kind> {
        match s {
            "commit" => Some(Kind::Commit),
            "tree" => Some(Kind::Tree),
            "blob" => Some(Kind::Blob),
            "tag" => Some(Kind::Tag),
            _ => None,
        }
    }
}

/// SHA-1 of `<type> <len>\0<content>` with collision detection: an object
/// built to collide (the SHAttered construction) is refused, never stored.
pub struct ObjHasher(sha1_checked::Sha1);

impl ObjHasher {
    pub fn new(kind: Kind, len: u64) -> ObjHasher {
        use sha1_checked::Digest;
        let mut h = sha1_checked::Sha1::new();
        h.update(format!("{} {}\0", kind.name(), len).as_bytes());
        ObjHasher(h)
    }
    pub fn update(&mut self, b: &[u8]) {
        use sha1_checked::Digest;
        self.0.update(b);
    }
    pub fn finish(self) -> Result<Oid, String> {
        match self.0.try_finalize() {
            sha1_checked::CollisionResult::Ok(d) => Ok(Oid(d.into())),
            _ => Err("object refused: SHA-1 collision attack detected".into()),
        }
    }
}

pub fn object_id(kind: Kind, content: &[u8]) -> Result<Oid, String> {
    let mut h = ObjHasher::new(kind, content.len() as u64);
    h.update(content);
    h.finish()
}

/// git check-ref-format, for full names under refs/.
pub fn valid_refname(name: &str) -> bool {
    if !name.starts_with("refs/") || name.len() > 1024 || name.ends_with('/') {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") || name.ends_with('.') {
        return false;
    }
    for c in name.bytes() {
        if c < 0x20 || c == 0x7f || b" ~^:?*[\\".contains(&c) {
            return false;
        }
    }
    name.split('/')
        .all(|comp| !comp.is_empty() && !comp.starts_with('.') && !comp.ends_with(".lock"))
        && name != "refs/"
}

/// Glob with `*` (any run, including `/`) — the ACL and protection pattern
/// language. No other metacharacters.
pub fn glob(pat: &str, s: &str) -> bool {
    let p = pat.as_bytes();
    let t = s.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_hex_roundtrip() {
        let h = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let o = Oid::from_hex(h).unwrap();
        assert_eq!(o.hex(), h);
        assert!(Oid::from_hex("zz").is_none());
        // the empty blob
        assert_eq!(object_id(Kind::Blob, b"").unwrap().hex(), h);
        assert_eq!(
            object_id(Kind::Blob, b"hello\n").unwrap().hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn refnames() {
        for ok in [
            "refs/heads/main",
            "refs/tags/v1.0",
            "refs/heads/feature/x-y_z",
            "refs/notes/commits",
        ] {
            assert!(valid_refname(ok), "{ok}");
        }
        for bad in [
            "HEAD",
            "refs/",
            "refs/heads/",
            "refs/heads/a..b",
            "refs/heads/.x",
            "refs/heads/x.lock",
            "refs/heads/a b",
            "refs/heads/a~1",
            "refs/heads/a^",
            "refs/heads/a:b",
            "refs/heads/a@{1}",
            "refs/heads//a",
            "refs/heads/a.",
            "refs/heads/a\\b",
            "main",
        ] {
            assert!(!valid_refname(bad), "{bad}");
        }
    }

    #[test]
    fn globs() {
        assert!(glob("*", "anything/at/all"));
        assert!(glob("enclave", "enclave"));
        assert!(!glob("enclave", "enclave-apps"));
        assert!(glob("enclave*", "enclave-apps"));
        assert!(glob("team/*", "team/x"));
        assert!(!glob("team/*", "other/x"));
        assert!(glob("refs/tags/*", "refs/tags/v1"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }
}
