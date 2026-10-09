//! index-pack: take a pushed pack apart into objects the server can name.
//!
//! Pass one runs while the body is still arriving: entry headers are parsed
//! and every zlib stream inflated as bytes land, whole objects are hashed
//! (SHA-1 with collision detection) and commits/trees/tags read for their
//! links, and the trailer checksum accumulates. Pass two resolves deltas
//! depth-first from each base, under a byte budget for the base cache
//! (an evicted base is rebuilt from its chain). A REF_DELTA against an
//! object the client assumed we hold (a thin pack) is resolved from the
//! repository, and that base is appended to the pack as a whole object so
//! every stored pack stands alone. Pass two runs in slices so the event
//! loop keeps serving other clients while a large push resolves.

use super::object::{links, Links};
use super::pack::{self, Base, EntryHeader, REF_DELTA};
use super::segbuf::SegBuf;
use super::{delta, Kind, ObjHasher, Oid};
use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};
use sha1::Digest;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Instant;

/// One object of the received pack, as the store's index records it.
#[derive(Clone)]
pub struct IdxEntry {
    pub oid: Oid,
    pub offset: u64,
    pub len: u64,
    /// as stored: 1..=4, OFS_DELTA or REF_DELTA
    pub stored: u8,
    pub base: Option<Oid>,
    pub kind: Kind,
    pub size: u64,
    pub time: i64,
    pub children: Vec<(Kind, Oid)>,
}

struct Entry {
    offset: u64,
    len: u64,
    hdr: EntryHeader,
    kind: Option<Kind>,
    size: u64,
    oid: Oid,
    links: Option<Links>,
}

struct Partial {
    offset: u64,
    hdr: EntryHeader,
    st: Box<InflateState>,
    inpos: usize,
    out: u64,
    hasher: Option<ObjHasher>,
    collect: Option<Vec<u8>>,
}

pub struct Limits {
    pub max_pack: u64,
    pub max_object: u64,
}

pub struct Ingest {
    pub buf: SegBuf,
    limits: Limits,
    count: Option<u32>,
    entries: Vec<Entry>,
    cur: Option<Partial>,
    next: usize,
    hashed: usize,
    trailer: sha1::Sha1,
    parsed_all: bool,
    finished_input: bool,
    resolve: Option<Resolver>,
    ext: Vec<(Oid, Kind, Rc<Vec<u8>>)>,
    pub resolved_deltas: usize,
    pub total_deltas: usize,
}

/// What pass two needs from the repository for a thin pack's bases.
pub trait Bases {
    fn base(&mut self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>, String>;
}

struct Resolver {
    by_offset: HashMap<u64, usize>,
    kids_ofs: HashMap<usize, Vec<usize>>,
    kids_ref: HashMap<Oid, Vec<usize>>,
    by_oid: HashMap<Oid, usize>,
    roots: VecDeque<Root>,
    stack: Vec<Node>,
    cache: Cache,
    unresolved: usize,
    missing: std::collections::HashSet<Oid>,
}

#[derive(Clone, Copy)]
enum Root {
    Entry(usize),
    Ext(usize),
}

#[derive(Clone, Copy)]
enum Node {
    Entry(usize),
    Ext(usize),
}

/// Content of resolved objects, keyed by entry index, LRU under a budget.
struct Cache {
    map: HashMap<usize, (Rc<Vec<u8>>, u64)>,
    order: VecDeque<(usize, u64)>,
    tick: u64,
    bytes: usize,
    budget: usize,
}

impl Cache {
    fn get(&mut self, i: usize) -> Option<Rc<Vec<u8>>> {
        let (c, _) = self.map.get(&i)?;
        let c = c.clone();
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(&i).unwrap().1 = t;
        self.order.push_back((i, t));
        Some(c)
    }
    fn put(&mut self, i: usize, c: Rc<Vec<u8>>) {
        self.tick += 1;
        if let Some((old, _)) = self.map.insert(i, (c.clone(), self.tick)) {
            self.bytes -= old.len();
        }
        self.bytes += c.len();
        self.order.push_back((i, self.tick));
        while self.bytes > self.budget {
            let Some((k, t)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&k).map(|e| e.1) == Some(t) {
                let (v, _) = self.map.remove(&k).unwrap();
                self.bytes -= v.len();
            }
        }
        if self.order.len() > 4 * self.map.len() + 1024 {
            let map = &self.map;
            self.order
                .retain(|(k, t)| map.get(k).map(|e| e.1) == Some(*t));
        }
    }
    fn drop_entry(&mut self, i: usize) {
        if let Some((v, _)) = self.map.remove(&i) {
            self.bytes -= v.len();
        }
    }
}

const SCRATCH: usize = 64 * 1024;

impl Ingest {
    pub fn new(limits: Limits) -> Ingest {
        Ingest {
            buf: SegBuf::new(),
            limits,
            count: None,
            entries: Vec::new(),
            cur: None,
            next: 12,
            hashed: 0,
            trailer: sha1::Sha1::new(),
            parsed_all: false,
            finished_input: false,
            resolve: None,
            ext: Vec::new(),
            resolved_deltas: 0,
            total_deltas: 0,
        }
    }

    pub fn objects(&self) -> u32 {
        self.count.unwrap_or(0)
    }

    /// Pass one over newly arrived bytes.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), String> {
        if self.buf.len() as u64 + data.len() as u64 > self.limits.max_pack {
            return Err(format!(
                "pack exceeds this server's {} MiB push limit; push history in smaller pieces",
                self.limits.max_pack >> 20
            ));
        }
        self.buf.push(data);
        self.advance()
    }

    fn advance(&mut self) -> Result<(), String> {
        if self.count.is_none() {
            if self.buf.len() < 12 {
                return Ok(());
            }
            let head = self.buf.slice(0, 12);
            if &head[..4] != b"PACK" {
                return Err("not a pack".into());
            }
            let v = u32::from_be_bytes(head[4..8].try_into().unwrap());
            if v != 2 && v != 3 {
                return Err(format!("unsupported pack version {v}"));
            }
            let n = u32::from_be_bytes(head[8..12].try_into().unwrap());
            if n as u64 > self.limits.max_pack / 8 + 16 {
                return Err("pack claims more objects than its size allows".into());
            }
            self.count = Some(n);
        }
        let count = self.count.unwrap() as usize;
        let mut scratch = vec![0u8; SCRATCH];
        while self.entries.len() < count {
            if self.cur.is_none() {
                let off = self.next as u64;
                let peek = self
                    .buf
                    .slice(self.next, (self.next + 64).min(self.buf.len()));
                let Some(hdr) = pack::parse_header(&peek, off)? else {
                    break;
                };
                if hdr.size > self.limits.max_object {
                    return Err(format!(
                        "an object exceeds this server's {} MiB object limit",
                        self.limits.max_object >> 20
                    ));
                }
                let (hasher, collect) = match hdr.kind {
                    k @ 1..=4 => {
                        let kind = Kind::from_u8(k).unwrap();
                        let collect = if kind == Kind::Blob {
                            None
                        } else {
                            Some(Vec::with_capacity(hdr.size.min(1 << 20) as usize))
                        };
                        (Some(ObjHasher::new(kind, hdr.size)), collect)
                    }
                    _ => (None, None),
                };
                self.cur = Some(Partial {
                    offset: off,
                    hdr,
                    st: InflateState::new_boxed(DataFormat::Zlib),
                    inpos: self.next + hdr.header_len,
                    out: 0,
                    hasher,
                    collect,
                });
            }
            let p = self.cur.as_mut().unwrap();
            let mut ended = false;
            loop {
                if p.inpos >= self.buf.len() {
                    break; // wait for more bytes
                }
                let r = inflate(
                    &mut p.st,
                    self.buf.piece(p.inpos),
                    &mut scratch,
                    MZFlush::None,
                );
                p.inpos += r.bytes_consumed;
                let out = &scratch[..r.bytes_written];
                p.out += out.len() as u64;
                if p.out > p.hdr.size {
                    return Err("pack entry inflates past its declared size".into());
                }
                if let Some(h) = &mut p.hasher {
                    h.update(out);
                }
                if let Some(c) = &mut p.collect {
                    c.extend_from_slice(out);
                }
                match r.status {
                    Ok(MZStatus::StreamEnd) => {
                        ended = true;
                        break;
                    }
                    Ok(_) if r.bytes_consumed == 0 && r.bytes_written == 0 => break,
                    Ok(_) => {}
                    Err(miniz_oxide::MZError::Buf) => break,
                    Err(_) => return Err("corrupt zlib stream in pack".into()),
                }
                if p.inpos == self.buf.len() && r.bytes_written < scratch.len() {
                    break;
                }
            }
            if !ended {
                break; // wait for more bytes
            }
            let p = self.cur.take().unwrap();
            if p.out != p.hdr.size {
                return Err("pack entry size mismatch".into());
            }
            let len = p.inpos as u64 - p.offset;
            let mut e = Entry {
                offset: p.offset,
                len,
                hdr: p.hdr,
                kind: None,
                size: p.hdr.size,
                oid: Oid::default(),
                links: None,
            };
            if let Some(h) = p.hasher {
                let kind = Kind::from_u8(p.hdr.kind).unwrap();
                e.kind = Some(kind);
                e.oid = h.finish()?;
                if let Some(c) = p.collect {
                    e.links = Some(links(kind, &c)?);
                }
            } else {
                self.total_deltas += 1;
            }
            self.entries.push(e);
            self.next = p.inpos;
        }
        // Everything before the next unparsed entry is pack body, never trailer.
        if self.next > self.hashed {
            let t = &mut self.trailer;
            self.buf.each(self.hashed, self.next, |p| t.update(p));
            self.hashed = self.next;
        }
        if self.entries.len() == count && self.cur.is_none() && !self.parsed_all {
            if self.buf.len() >= self.next + 20 {
                let digest: [u8; 20] = std::mem::take(&mut self.trailer).finalize().into();
                if *self.buf.slice(self.next, self.next + 20) != digest {
                    return Err("pack checksum mismatch".into());
                }
                self.parsed_all = true;
            }
        }
        if self.parsed_all && self.buf.len() > self.next + 20 {
            return Err("trailing garbage after pack".into());
        }
        Ok(())
    }

    /// The body ended: everything must be in.
    pub fn finish_input(&mut self) -> Result<(), String> {
        self.finished_input = true;
        if self.count.is_none() {
            return Err("truncated pack header".into());
        }
        if !self.parsed_all {
            return Err("truncated pack".into());
        }
        Ok(())
    }

    fn entry_content(&self, i: usize) -> Result<Vec<u8>, String> {
        let e = &self.entries[i];
        let start = e.offset as usize + e.hdr.header_len;
        let end = (e.offset + e.len) as usize;
        Ok(pack::inflate_at(&self.buf.slice(start, end), e.hdr.size)?.0)
    }

    fn setup(&mut self) -> Resolver {
        let mut r = Resolver {
            by_offset: HashMap::with_capacity(self.entries.len()),
            kids_ofs: HashMap::new(),
            kids_ref: HashMap::new(),
            by_oid: HashMap::with_capacity(self.entries.len()),
            roots: VecDeque::new(),
            stack: Vec::new(),
            cache: Cache {
                map: HashMap::new(),
                order: VecDeque::new(),
                tick: 0,
                bytes: 0,
                budget: 256 << 20,
            },
            unresolved: 0,
            missing: Default::default(),
        };
        for (i, e) in self.entries.iter().enumerate() {
            r.by_offset.insert(e.offset, i);
        }
        for (i, e) in self.entries.iter().enumerate() {
            match e.hdr.base {
                Base::None => {
                    r.by_oid.entry(e.oid).or_insert(i);
                    r.roots.push_back(Root::Entry(i));
                }
                Base::Ofs(o) => {
                    r.unresolved += 1;
                    if let Some(&b) = r.by_offset.get(&o) {
                        r.kids_ofs.entry(b).or_default().push(i);
                    }
                }
                Base::Ref(o) => {
                    r.unresolved += 1;
                    r.kids_ref.entry(o).or_default().push(i);
                }
            }
        }
        r
    }

    /// Content of entry `i`, which must be resolved; rebuilt along its delta
    /// chain when evicted.
    fn content(&self, r: &mut Resolver, i: usize) -> Result<Rc<Vec<u8>>, String> {
        if let Some(c) = r.cache.get(i) {
            return Ok(c);
        }
        // Walk to the nearest cached or whole ancestor, then replay forward.
        let mut chain = vec![i];
        let mut start: Option<Rc<Vec<u8>>> = None;
        loop {
            let cur = *chain.last().unwrap();
            let e = &self.entries[cur];
            let base = match e.hdr.base {
                Base::None => break,
                Base::Ofs(o) => *r.by_offset.get(&o).ok_or("delta base missing")?,
                Base::Ref(o) => match r.by_oid.get(&o) {
                    Some(&b) => b,
                    None => {
                        let x = self
                            .ext
                            .iter()
                            .find(|(eo, _, _)| *eo == o)
                            .ok_or("delta base missing")?;
                        start = Some(x.2.clone());
                        break;
                    }
                },
            };
            if let Some(c) = r.cache.get(base) {
                start = Some(c);
                break;
            }
            chain.push(base);
            if chain.len() > 10_000 {
                return Err("delta chain too deep".into());
            }
        }
        let mut cur: Rc<Vec<u8>> = match start {
            Some(s) => s,
            None => Rc::new(self.entry_content(chain.pop().unwrap())?),
        };
        while let Some(j) = chain.pop() {
            let d = self.entry_content(j)?;
            cur = Rc::new(delta::apply(&cur, &d, self.limits.max_object)?);
        }
        Ok(cur)
    }

    fn children_of(&self, r: &Resolver, n: Node) -> Vec<usize> {
        let mut v = Vec::new();
        let oid = match n {
            Node::Entry(i) => {
                if let Some(k) = r.kids_ofs.get(&i) {
                    v.extend_from_slice(k);
                }
                self.entries[i].oid
            }
            Node::Ext(x) => self.ext[x].0,
        };
        if let Some(k) = r.kids_ref.get(&oid) {
            v.extend(
                k.iter()
                    .copied()
                    .filter(|&c| self.entries[c].kind.is_none()),
            );
        }
        v
    }

    /// Pass two, in slices: returns Ok(true) once every delta is resolved.
    pub fn resolve(&mut self, deadline: Instant, bases: &mut dyn Bases) -> Result<bool, String> {
        if !self.parsed_all {
            return Err("pack incomplete".into());
        }
        let mut r = match self.resolve.take() {
            Some(r) => r,
            None => self.setup(),
        };
        let res = self.resolve_inner(&mut r, deadline, bases);
        self.resolve = Some(r);
        res
    }

    fn resolve_inner(
        &mut self,
        r: &mut Resolver,
        deadline: Instant,
        bases: &mut dyn Bases,
    ) -> Result<bool, String> {
        loop {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let node = match r.stack.pop() {
                Some(n) => n,
                None => match r.roots.pop_front() {
                    Some(Root::Entry(i)) => Node::Entry(i),
                    Some(Root::Ext(x)) => Node::Ext(x),
                    None => {
                        if r.unresolved == 0 {
                            return Ok(true);
                        }
                        // Thin pack: REF bases outside the pack come from the
                        // repository. A base can also be an in-pack delta that
                        // resolves only after an outside base arrives, so a miss
                        // is an error only when a whole round finds nothing.
                        let mut wanted: Vec<Oid> = Vec::new();
                        for (o, kids) in &r.kids_ref {
                            if !r.by_oid.contains_key(o)
                                && !r.missing.contains(o)
                                && kids.iter().any(|&k| self.entries[k].kind.is_none())
                                && !self.ext.iter().any(|(eo, _, _)| eo == o)
                            {
                                wanted.push(*o);
                            }
                        }
                        wanted.sort();
                        let mut found = 0;
                        for o in wanted {
                            match bases.base(&o)? {
                                Some((k, data)) => {
                                    self.ext.push((o, k, Rc::new(data)));
                                    r.roots.push_back(Root::Ext(self.ext.len() - 1));
                                    found += 1;
                                }
                                None => {
                                    r.missing.insert(o);
                                }
                            }
                        }
                        if found == 0 {
                            return Err(match r.missing.iter().next() {
                                Some(o) => format!(
                                    "pack refers to {o}, which this repository does not have"
                                ),
                                None => format!("pack has {} unresolvable deltas", r.unresolved),
                            });
                        }
                        continue;
                    }
                },
            };
            let kids = self.children_of(r, node);
            if kids.is_empty() {
                if let Node::Entry(i) = node {
                    r.cache.drop_entry(i);
                }
                continue;
            }
            let (base_content, base_kind) = match node {
                Node::Entry(i) => (self.content(r, i)?, self.entries[i].kind.unwrap()),
                Node::Ext(x) => (self.ext[x].2.clone(), self.ext[x].1),
            };
            for c in kids {
                if self.entries[c].kind.is_some() {
                    continue; // a duplicate REF child already resolved through a twin base
                }
                let d = self.entry_content(c)?;
                let out = delta::apply(&base_content, &d, self.limits.max_object)?;
                let oid = super::object_id(base_kind, &out)?;
                let lk = match base_kind {
                    Kind::Blob => None,
                    k => Some(links(k, &out)?),
                };
                let e = &mut self.entries[c];
                e.kind = Some(base_kind);
                e.size = out.len() as u64;
                e.oid = oid;
                e.links = lk;
                r.by_oid.entry(oid).or_insert(c);
                r.unresolved -= 1;
                self.resolved_deltas += 1;
                let has_kids = r.kids_ofs.contains_key(&c) || r.kids_ref.contains_key(&oid);
                if has_kids {
                    r.cache.put(c, Rc::new(out));
                    r.stack.push(Node::Entry(c));
                }
            }
            if let Node::Entry(i) = node {
                r.cache.drop_entry(i);
            }
        }
    }

    /// Append the thin-pack bases as whole objects, rewrite the count and the
    /// trailer, and describe every object for the store's index.
    pub fn complete(mut self) -> Result<(SegBuf, Vec<IdxEntry>), String> {
        let count = self.count.unwrap_or(0) as usize;
        if self.entries.len() != count || self.entries.iter().any(|e| e.kind.is_none()) {
            return Err("pack not fully resolved".into());
        }
        let mut out: Vec<IdxEntry> = Vec::with_capacity(count + self.ext.len());
        let mut by_offset: HashMap<u64, Oid> = HashMap::with_capacity(count);
        for e in &self.entries {
            by_offset.insert(e.offset, e.oid);
        }
        for e in self.entries.drain(..) {
            let base = match e.hdr.base {
                Base::None => None,
                Base::Ofs(o) => Some(*by_offset.get(&o).ok_or("delta base missing")?),
                Base::Ref(o) => Some(o),
            };
            let (time, children) = match e.links {
                Some(l) => (l.time, l.children),
                None => (0, Vec::new()),
            };
            out.push(IdxEntry {
                oid: e.oid,
                offset: e.offset,
                len: e.len,
                stored: e.hdr.kind,
                base,
                kind: e.kind.unwrap(),
                size: e.size,
                time,
                children,
            });
        }
        let mut buf = std::mem::take(&mut self.buf);
        buf.truncate(self.next); // drop the old trailer
        let mut seen: std::collections::HashSet<Oid> = out.iter().map(|e| e.oid).collect();
        for (oid, kind, data) in &self.ext {
            if !seen.insert(*oid) {
                continue;
            }
            let offset = buf.len() as u64;
            let mut e = Vec::new();
            pack::encode_header(*kind as u8, data.len() as u64, &mut e);
            e.extend_from_slice(&pack::deflate(data));
            buf.push(&e);
            let lk = match kind {
                Kind::Blob => None,
                k => Some(links(*k, data)?),
            };
            let (time, children) = match lk {
                Some(l) => (l.time, l.children),
                None => (0, Vec::new()),
            };
            out.push(IdxEntry {
                oid: *oid,
                offset,
                len: buf.len() as u64 - offset,
                stored: *kind as u8,
                base: None,
                kind: *kind,
                size: data.len() as u64,
                time,
                children,
            });
        }
        let n = out.len() as u32;
        if buf.len() >= 12 {
            buf.write_at(8, &n.to_be_bytes());
        }
        let mut h = sha1::Sha1::new();
        buf.each(0, buf.len(), |p| h.update(p));
        let digest: [u8; 20] = h.finalize().into();
        buf.push(&digest);
        // Refuse a stored REF delta whose base is not in this pack: packs stand alone.
        let have: std::collections::HashSet<Oid> = out.iter().map(|e| e.oid).collect();
        for e in &out {
            if e.stored == REF_DELTA && !have.contains(e.base.as_ref().unwrap()) {
                return Err("internal: thin base not appended".into());
            }
        }
        Ok((buf, out))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::git::object_id;
    use crate::git::pack::OFS_DELTA;

    pub struct NoBases;
    impl Bases for NoBases {
        fn base(&mut self, _: &Oid) -> Result<Option<(Kind, Vec<u8>)>, String> {
            Ok(None)
        }
    }

    pub fn enc_varint(mut v: u64, out: &mut Vec<u8>) {
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

    /// A delta that rewrites `base` into `target` as one insert per <=127 bytes.
    pub fn naive_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        enc_varint(base.len() as u64, &mut d);
        enc_varint(target.len() as u64, &mut d);
        // copy the common prefix, insert the rest
        let common = base
            .iter()
            .zip(target)
            .take_while(|(a, b)| a == b)
            .count()
            .min(0xffff);
        if common > 0 {
            d.push(0x80 | 0x10 | 0x20);
            d.push((common & 0xff) as u8);
            d.push((common >> 8) as u8);
        }
        for ch in target[common..].chunks(127) {
            d.push(ch.len() as u8);
            d.extend_from_slice(ch);
        }
        d
    }

    pub enum T<'a> {
        Whole(Kind, &'a [u8]),
        Ofs(usize, Vec<u8>),
        Ref(Oid, Vec<u8>),
    }

    pub fn build_pack(items: &[T]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"PACK");
        b.extend_from_slice(&2u32.to_be_bytes());
        b.extend_from_slice(&(items.len() as u32).to_be_bytes());
        let mut offs = Vec::new();
        for it in items {
            let off = b.len() as u64;
            offs.push(off);
            match it {
                T::Whole(k, d) => {
                    pack::encode_header(*k as u8, d.len() as u64, &mut b);
                    b.extend_from_slice(&pack::deflate(d));
                }
                T::Ofs(i, d) => {
                    pack::encode_header(OFS_DELTA, d.len() as u64, &mut b);
                    pack::encode_ofs(off - offs[*i], &mut b);
                    b.extend_from_slice(&pack::deflate(d));
                }
                T::Ref(o, d) => {
                    pack::encode_header(REF_DELTA, d.len() as u64, &mut b);
                    b.extend_from_slice(&o.0);
                    b.extend_from_slice(&pack::deflate(d));
                }
            }
        }
        let dg: [u8; 20] = sha1::Sha1::digest(&b).into();
        b.extend_from_slice(&dg);
        b
    }

    fn lim() -> Limits {
        Limits {
            max_pack: 1 << 30,
            max_object: 1 << 28,
        }
    }

    fn run(
        pack: &[u8],
        step: usize,
        bases: &mut dyn Bases,
    ) -> Result<(Vec<u8>, Vec<IdxEntry>), String> {
        let (b, i) = run_seg(pack, step, bases)?;
        Ok((b.to_vec(), i))
    }

    fn run_seg(
        pack: &[u8],
        step: usize,
        bases: &mut dyn Bases,
    ) -> Result<(SegBuf, Vec<IdxEntry>), String> {
        let mut ing = Ingest::new(lim());
        for ch in pack.chunks(step) {
            ing.feed(ch)?;
        }
        ing.finish_input()?;
        while !ing.resolve(Instant::now() + std::time::Duration::from_secs(5), bases)? {}
        ing.complete()
    }

    #[test]
    fn whole_and_delta_chain_any_feed_size() {
        let a = b"line one\nline two\nline three\n".repeat(40);
        let mut b2 = a.clone();
        b2.extend_from_slice(b"more\n");
        let mut c = b2.clone();
        c.extend_from_slice(b"even more\n");
        let oa = object_id(Kind::Blob, &a).unwrap();
        let ob = object_id(Kind::Blob, &b2).unwrap();
        let oc = object_id(Kind::Blob, &c).unwrap();
        let pack = build_pack(&[
            T::Whole(Kind::Blob, &a),
            T::Ofs(0, naive_delta(&a, &b2)),
            T::Ref(ob, naive_delta(&b2, &c)),
        ]);
        for step in [1, 7, 100, 1 << 20] {
            let (out, idx) = run(&pack, step, &mut NoBases).unwrap();
            assert_eq!(out, pack, "nothing appended: pack is unchanged");
            let oids: Vec<Oid> = idx.iter().map(|e| e.oid).collect();
            assert_eq!(oids, vec![oa, ob, oc]);
            assert_eq!(idx[1].base, Some(oa));
            assert_eq!(idx[2].base, Some(ob));
            assert_eq!(idx[2].size, c.len() as u64);
        }
    }

    struct OneBase(Oid, Vec<u8>);
    impl Bases for OneBase {
        fn base(&mut self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>, String> {
            Ok((*oid == self.0).then(|| (Kind::Blob, self.1.clone())))
        }
    }

    #[test]
    fn thin_pack_gets_its_base_appended() {
        let base = b"the base object, held by the server\n".repeat(10);
        let ob = object_id(Kind::Blob, &base).unwrap();
        let mut t = base.clone();
        t.extend_from_slice(b"pushed change\n");
        let ot = object_id(Kind::Blob, &t).unwrap();
        let pack = build_pack(&[T::Ref(ob, naive_delta(&base, &t))]);
        assert!(run(&pack, 13, &mut NoBases).is_err(), "no base, no pack");
        let (out, idx) = run(&pack, 13, &mut OneBase(ob, base.clone())).unwrap();
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].oid, ot);
        assert_eq!(idx[1].oid, ob);
        // the completed pack is self-contained and checks out
        let (out2, idx2) = run(&out, 1 << 20, &mut NoBases).unwrap();
        assert_eq!(out2, out);
        assert_eq!(idx2.len(), 2);
    }

    #[test]
    fn corrupt_packs_are_refused() {
        let pack = build_pack(&[T::Whole(Kind::Blob, b"x")]);
        let mut bad = pack.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(run(&bad, 64, &mut NoBases).is_err(), "trailer");
        let mut lie = pack.clone();
        lie[11] = 2; // claims two objects
        assert!(run(&lie, 64, &mut NoBases).is_err());
        let mut extra = pack.clone();
        extra.push(0);
        assert!(run(&extra, 64, &mut NoBases).is_err(), "trailing bytes");
        assert!(
            run(&pack[..pack.len() - 3], 64, &mut NoBases).is_err(),
            "truncated"
        );
        let mut small = Ingest::new(Limits {
            max_pack: 10,
            max_object: 10,
        });
        assert!(small.feed(&pack).is_err());
    }

    #[test]
    fn deep_chain_under_tiny_cache() {
        // 300 successive versions, each a delta on the previous one
        let mut versions = vec![b"v0\n".to_vec()];
        for i in 1..300 {
            let mut n = versions[i - 1].clone();
            n.extend_from_slice(format!("v{i}\n").as_bytes());
            versions.push(n);
        }
        let mut items = vec![T::Whole(Kind::Blob, &versions[0])];
        for i in 1..300 {
            items.push(T::Ofs(i - 1, naive_delta(&versions[i - 1], &versions[i])));
        }
        let pack = build_pack(&items);
        let mut ing = Ingest::new(lim());
        ing.feed(&pack).unwrap();
        ing.finish_input().unwrap();
        let mut r = ing.setup();
        r.cache.budget = 64; // forces rebuilds along the chain
        ing.resolve = Some(r);
        while !ing
            .resolve(
                Instant::now() + std::time::Duration::from_secs(5),
                &mut NoBases,
            )
            .unwrap()
        {}
        let (_, idx) = ing.complete().unwrap();
        assert_eq!(idx[299].oid, object_id(Kind::Blob, &versions[299]).unwrap());
    }
}
