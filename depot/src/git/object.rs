//! Reading commits, trees and tags: the links between objects (what the
//! server's graph needs) and the fields the web view shows.

use super::{Kind, Oid};

/// What an object points at, with the type each target must have, and
/// (commits) the committer time that orders history walks.
pub struct Links {
    pub children: Vec<(Kind, Oid)>,
    pub time: i64,
}

pub const MODE_TREE: u32 = 0o40000;
pub const MODE_GITLINK: u32 = 0o160000;

pub struct TreeEntry<'a> {
    pub mode: u32,
    pub name: &'a [u8],
    pub oid: Oid,
}

/// Iterate a tree's `<octal mode> <name>\0<20-byte id>` records.
pub fn tree_entries(data: &[u8]) -> impl Iterator<Item = Result<TreeEntry<'_>, String>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= data.len() {
            return None;
        }
        let rest = &data[pos..];
        let Some(sp) = rest.iter().position(|&b| b == b' ') else {
            pos = data.len();
            return Some(Err("malformed tree entry (mode)".into()));
        };
        let Some(nul) = rest.iter().position(|&b| b == 0) else {
            pos = data.len();
            return Some(Err("malformed tree entry (name)".into()));
        };
        if nul < sp || rest.len() < nul + 21 || sp == 0 || sp > 7 || nul == sp + 1 {
            pos = data.len();
            return Some(Err("malformed tree entry".into()));
        }
        let mut mode = 0u32;
        for &c in &rest[..sp] {
            if !(b'0'..=b'7').contains(&c) {
                pos = data.len();
                return Some(Err("malformed tree entry mode".into()));
            }
            mode = mode * 8 + (c - b'0') as u32;
        }
        let name = &rest[sp + 1..nul];
        let oid = Oid::from_slice(&rest[nul + 1..nul + 21]).unwrap_or_default();
        pos += nul + 21;
        Some(Ok(TreeEntry { mode, name, oid }))
    })
}

fn header_oid(line: &[u8], key: &[u8]) -> Option<Oid> {
    let rest = line.strip_prefix(key)?;
    Oid::from_hex(std::str::from_utf8(rest).ok()?)
}

/// `<name> <email> <unix> <tz>` -> unix seconds.
pub fn ident_time(ident: &[u8]) -> i64 {
    let s = String::from_utf8_lossy(ident);
    let mut it = s.rsplitn(3, ' ');
    let _tz = it.next();
    it.next().and_then(|t| t.parse().ok()).unwrap_or(0)
}

pub fn links(kind: Kind, data: &[u8]) -> Result<Links, String> {
    let mut children = Vec::new();
    let mut time = 0i64;
    match kind {
        Kind::Blob => {}
        Kind::Tree => {
            for e in tree_entries(data) {
                let e = e?;
                match e.mode {
                    MODE_GITLINK => {} // a submodule's commit lives in another repository
                    MODE_TREE => children.push((Kind::Tree, e.oid)),
                    _ => children.push((Kind::Blob, e.oid)),
                }
            }
        }
        Kind::Commit => {
            let mut lines = data.split(|&b| b == b'\n');
            let first = lines.next().unwrap_or(b"");
            let tree = header_oid(first, b"tree ").ok_or("malformed commit: no tree")?;
            children.push((Kind::Tree, tree));
            for l in lines {
                if l.is_empty() {
                    break;
                }
                if let Some(p) = l.strip_prefix(b"parent ") {
                    let p = std::str::from_utf8(p).ok().and_then(Oid::from_hex);
                    children.push((Kind::Commit, p.ok_or("malformed commit parent")?));
                } else if let Some(c) = l.strip_prefix(b"committer ") {
                    time = ident_time(c);
                }
            }
        }
        Kind::Tag => {
            let mut target = None;
            let mut tkind = None;
            for l in data.split(|&b| b == b'\n') {
                if l.is_empty() {
                    break;
                }
                if let Some(o) = header_oid(l, b"object ") {
                    target = Some(o);
                } else if let Some(t) = l.strip_prefix(b"type ") {
                    tkind = std::str::from_utf8(t).ok().and_then(Kind::from_name);
                } else if let Some(t) = l.strip_prefix(b"tagger ") {
                    time = ident_time(t);
                }
            }
            match (target, tkind) {
                (Some(o), Some(k)) => children.push((k, o)),
                _ => return Err("malformed tag".into()),
            }
        }
    }
    Ok(Links { children, time })
}

/// The parts of a commit the web view shows.
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

pub fn parse_commit(data: &[u8]) -> Result<Commit, String> {
    let split = data
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, i + 2))
        .unwrap_or((data.len(), data.len()));
    let mut tree = None;
    let mut parents = Vec::new();
    let (mut author, mut committer) = (String::new(), String::new());
    for l in data[..split.0].split(|&b| b == b'\n') {
        if let Some(o) = header_oid(l, b"tree ") {
            tree = Some(o);
        } else if let Some(o) = header_oid(l, b"parent ") {
            parents.push(o);
        } else if let Some(a) = l.strip_prefix(b"author ") {
            author = String::from_utf8_lossy(a).into_owned();
        } else if let Some(c) = l.strip_prefix(b"committer ") {
            committer = String::from_utf8_lossy(c).into_owned();
        }
    }
    Ok(Commit {
        tree: tree.ok_or("malformed commit")?,
        parents,
        author,
        committer,
        message: String::from_utf8_lossy(&data[split.1..]).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_links() {
        let c = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent ce013625030ba8dba906f756967f9e9ca394464a\nauthor A <a@x> 1700000000 +0000\ncommitter C <c@x> 1700000123 -0700\n\nmsg\n";
        let l = links(Kind::Commit, c).unwrap();
        assert_eq!(l.children.len(), 2);
        assert_eq!(l.children[0].0, Kind::Tree);
        assert_eq!(l.children[1].0, Kind::Commit);
        assert_eq!(l.time, 1700000123);
        let p = parse_commit(c).unwrap();
        assert_eq!(p.message, "msg\n");
        assert_eq!(p.parents.len(), 1);
        assert!(links(Kind::Commit, b"nope\n").is_err());
    }

    #[test]
    fn tree_links_skip_gitlinks() {
        let mut t = Vec::new();
        for (mode, name, b) in [
            ("100644", "a", 1u8),
            ("40000", "d", 2),
            ("160000", "sub", 3),
            ("120000", "l", 4),
        ] {
            t.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            t.extend_from_slice(&[b; 20]);
        }
        let l = links(Kind::Tree, &t).unwrap();
        assert_eq!(l.children.len(), 3);
        assert_eq!(l.children[1], (Kind::Tree, Oid([2; 20])));
        assert!(links(Kind::Tree, b"100644 a\0short").is_err());
        assert!(links(Kind::Tree, b"1006x4 a\0aaaaaaaaaaaaaaaaaaaa").is_err());
    }

    #[test]
    fn tag_links() {
        let t = b"object ce013625030ba8dba906f756967f9e9ca394464a\ntype blob\ntag v1\ntagger T <t@x> 5 +0000\n\nm";
        let l = links(Kind::Tag, t).unwrap();
        assert_eq!(l.children[0].0, Kind::Blob);
        assert_eq!(l.time, 5);
    }
}
