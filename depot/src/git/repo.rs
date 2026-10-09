//! The in-memory object index of one repository: where every object lives
//! (pack, offset, entry length, how it is stored), its type and size, and
//! its links, as indices into the same table. Fetch negotiation, history
//! walks and connectivity checks run entirely here; pack bytes are read
//! only to send them.
//!
//! The per-pack index object the store keeps (`encode`/`decode`) carries the
//! same fields, so loading a repository is decrypting its packs' indexes.

use super::ingest::IdxEntry;
use super::{Kind, Oid};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

pub const NONE: u32 = u32::MAX;

/// Object ids are uniformly random: their first eight bytes are the hash.
#[derive(Default)]
pub struct OidHasher(u64);
impl Hasher for OidHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().take(8) {
            self.0 = (self.0 << 8) | b as u64;
        }
    }
}
pub type OidMap<V> = HashMap<Oid, V, BuildHasherDefault<OidHasher>>;

#[derive(Debug)]
pub struct PackInfo {
    pub id: String,
    /// plaintext pack length (the encrypted object's chunking is derived from it)
    pub len: u64,
    /// encryption chunk size of the stored object
    pub chunk: u32,
}

#[derive(Clone, Copy)]
pub struct Obj {
    pub pack: u32,
    pub offset: u64,
    pub len: u64,
    pub stored: u8,
    pub kind: Kind,
    pub size: u64,
    pub base: u32,
    pub time: i64,
    pub kids: u32,
    pub nkids: u32,
}

#[derive(Default)]
pub struct Index {
    pub packs: Vec<Rc<PackInfo>>,
    pub oids: Vec<Oid>,
    pub objs: Vec<Obj>,
    pub map: OidMap<u32>,
    pub arena: Vec<u32>,
    /// children named by an object that the index does not hold (a corrupt
    /// or foreign store); walks refuse to cross them
    pub dangling: usize,
}

impl Index {
    pub fn lookup(&self, oid: &Oid) -> Option<u32> {
        self.map.get(oid).copied()
    }
    pub fn obj(&self, i: u32) -> &Obj {
        &self.objs[i as usize]
    }
    pub fn oid(&self, i: u32) -> Oid {
        self.oids[i as usize]
    }
    pub fn kids(&self, i: u32) -> &[u32] {
        let o = &self.objs[i as usize];
        &self.arena[o.kids as usize..(o.kids + o.nkids) as usize]
    }
    pub fn len(&self) -> usize {
        self.objs.len()
    }

    /// Add one stored pack. Its objects may link to each other in any order
    /// and to objects of packs added before it.
    pub fn add_pack(&mut self, info: Rc<PackInfo>, entries: &[IdxEntry]) -> Result<(), String> {
        let p = self.packs.len() as u32;
        self.packs.push(info);
        let first = self.objs.len();
        let mut added: Vec<usize> = Vec::with_capacity(entries.len());
        for (k, e) in entries.iter().enumerate() {
            if self.map.contains_key(&e.oid) {
                continue; // a duplicate: the first copy serves
            }
            let i = self.objs.len() as u32;
            self.map.insert(e.oid, i);
            self.oids.push(e.oid);
            self.objs.push(Obj {
                pack: p,
                offset: e.offset,
                len: e.len,
                stored: e.stored,
                kind: e.kind,
                size: e.size,
                base: NONE,
                time: e.time,
                kids: 0,
                nkids: 0,
            });
            added.push(k);
        }
        for (n, &k) in added.iter().enumerate() {
            let e = &entries[k];
            let i = first + n;
            let base = match &e.base {
                Some(b) => self
                    .lookup(b)
                    .ok_or("index names a delta base it does not hold")?,
                None => NONE,
            };
            if base == i as u32 {
                return Err(format!("index stores {} as a delta on itself", e.oid));
            }
            let start = self.arena.len() as u32;
            for (_, c) in &e.children {
                match self.lookup(c) {
                    Some(ci) => self.arena.push(ci),
                    None => {
                        self.arena.push(NONE);
                        self.dangling += 1;
                    }
                }
            }
            let o = &mut self.objs[i];
            o.base = base;
            o.kids = start;
            o.nkids = e.children.len() as u32;
        }
        Ok(())
    }

    /// The index entry of object `i`, as `encode` would record it.
    #[cfg(test)]
    pub fn entry(&self, i: u32) -> IdxEntry {
        let o = self.obj(i);
        IdxEntry {
            oid: self.oid(i),
            offset: o.offset,
            len: o.len,
            stored: o.stored,
            base: (o.base != NONE).then(|| self.oid(o.base)),
            kind: o.kind,
            size: o.size,
            time: o.time,
            children: self
                .kids(i)
                .iter()
                .map(|&c| {
                    if c == NONE {
                        (Kind::Blob, Oid::default())
                    } else {
                        (self.obj(c).kind, self.oid(c))
                    }
                })
                .collect(),
        }
    }

    /// Peel a tag chain: the first non-tag object it names.
    pub fn peel(&self, mut i: u32) -> Option<u32> {
        for _ in 0..64 {
            let o = self.obj(i);
            if o.kind != Kind::Tag {
                return Some(i);
            }
            let k = *self.kids(i).first()?;
            if k == NONE {
                return None;
            }
            i = k;
        }
        None
    }
}

fn put_varint(mut v: u64, out: &mut Vec<u8>) {
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

fn get_varint(b: &[u8], p: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let c = *b.get(*p).ok_or("truncated pack index")?;
        *p += 1;
        if shift > 63 {
            return Err("pack index varint overflow".into());
        }
        v |= ((c & 0x7f) as u64) << shift;
        shift += 7;
        if c & 0x80 == 0 {
            return Ok(v);
        }
    }
}

fn get_oid(b: &[u8], p: &mut usize) -> Result<Oid, String> {
    let o = Oid::from_slice(b.get(*p..*p + 20).ok_or("truncated pack index")?).unwrap();
    *p += 20;
    Ok(o)
}

const IDX_MAGIC: &[u8; 4] = b"DIX1";

/// The stored per-pack index: plaintext form (compressed and sealed by the store).
pub fn encode(entries: &[IdxEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 40);
    out.extend_from_slice(IDX_MAGIC);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&e.oid.0);
        put_varint(e.offset, &mut out);
        put_varint(e.len, &mut out);
        out.push(e.stored);
        out.push(e.kind as u8);
        put_varint(e.size, &mut out);
        if let Some(b) = &e.base {
            out.extend_from_slice(&b.0);
        }
        if matches!(e.kind, Kind::Commit | Kind::Tag) {
            put_varint(((e.time << 1) ^ (e.time >> 63)) as u64, &mut out);
        }
        if e.kind != Kind::Blob {
            put_varint(e.children.len() as u64, &mut out);
            for (k, c) in &e.children {
                out.push(*k as u8);
                out.extend_from_slice(&c.0);
            }
        }
    }
    out
}

pub fn decode(b: &[u8]) -> Result<Vec<IdxEntry>, String> {
    if b.len() < 8 || &b[..4] != IDX_MAGIC {
        return Err("not a depot pack index".into());
    }
    let n = u32::from_be_bytes(b[4..8].try_into().unwrap()) as usize;
    if n > b.len() / 24 + 1 {
        return Err("pack index count is implausible".into());
    }
    let mut p = 8;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let oid = get_oid(b, &mut p)?;
        let offset = get_varint(b, &mut p)?;
        let len = get_varint(b, &mut p)?;
        let stored = *b.get(p).ok_or("truncated pack index")?;
        let kind = Kind::from_u8(*b.get(p + 1).ok_or("truncated pack index")?)
            .ok_or("bad kind in pack index")?;
        p += 2;
        let size = get_varint(b, &mut p)?;
        let base = if stored == super::pack::OFS_DELTA || stored == super::pack::REF_DELTA {
            Some(get_oid(b, &mut p)?)
        } else {
            None
        };
        let time = if matches!(kind, Kind::Commit | Kind::Tag) {
            let z = get_varint(b, &mut p)?;
            ((z >> 1) as i64) ^ -((z & 1) as i64)
        } else {
            0
        };
        let mut children = Vec::new();
        if kind != Kind::Blob {
            let k = get_varint(b, &mut p)? as usize;
            if k > (b.len() - p) / 21 {
                return Err("truncated pack index".into());
            }
            children.reserve(k);
            for _ in 0..k {
                let ck = Kind::from_u8(*b.get(p).ok_or("truncated pack index")?)
                    .ok_or("bad kind in pack index")?;
                p += 1;
                children.push((ck, get_oid(b, &mut p)?));
            }
        }
        out.push(IdxEntry {
            oid,
            offset,
            len,
            stored,
            base,
            kind,
            size,
            time,
            children,
        });
    }
    if p != b.len() {
        return Err("trailing bytes in pack index".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(oid: u8, kind: Kind, kids: &[(Kind, u8)], base: Option<u8>) -> IdxEntry {
        IdxEntry {
            oid: Oid([oid; 20]),
            offset: oid as u64 * 100,
            len: 50,
            stored: if base.is_some() { 6 } else { kind as u8 },
            base: base.map(|b| Oid([b; 20])),
            kind,
            size: 77,
            time: -5 + oid as i64,
            children: kids.iter().map(|(k, o)| (*k, Oid([*o; 20]))).collect(),
        }
    }

    #[test]
    fn idx_roundtrip_and_links() {
        let entries = vec![
            e(1, Kind::Commit, &[(Kind::Tree, 2)], None),
            e(2, Kind::Tree, &[(Kind::Blob, 3), (Kind::Blob, 4)], None),
            e(3, Kind::Blob, &[], None),
            e(4, Kind::Blob, &[], Some(3)),
            e(5, Kind::Tag, &[(Kind::Commit, 1)], None),
        ];
        let enc = encode(&entries);
        let dec = decode(&enc).unwrap();
        assert_eq!(dec.len(), 5);
        assert_eq!(dec[0].time, -4);
        assert_eq!(dec[3].base, Some(Oid([3; 20])));
        assert!(decode(&enc[..enc.len() - 1]).is_err());
        let mut ix = Index::default();
        ix.add_pack(
            Rc::new(PackInfo {
                id: "p".into(),
                len: 1,
                chunk: 262144,
            }),
            &dec,
        )
        .unwrap();
        let c = ix.lookup(&Oid([1; 20])).unwrap();
        let t = ix.kids(c)[0];
        assert_eq!(ix.oid(t), Oid([2; 20]));
        assert_eq!(ix.kids(t).len(), 2);
        assert_eq!(ix.peel(ix.lookup(&Oid([5; 20])).unwrap()), Some(c));
        assert_eq!(ix.entry(t).children, entries[1].children);
        assert_eq!(ix.dangling, 0);
        // a second pack that repeats an object keeps the first copy
        ix.add_pack(
            Rc::new(PackInfo {
                id: "q".into(),
                len: 1,
                chunk: 262144,
            }),
            &dec[2..3],
        )
        .unwrap();
        assert_eq!(ix.len(), 5);
        assert_eq!(ix.obj(ix.lookup(&Oid([3; 20])).unwrap()).pack, 0);
    }
}
