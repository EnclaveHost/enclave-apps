//! pack-objects for fetches, by reuse: entries are copied from the stored
//! packs as they are, compressed bytes and deltas included. Objects go out
//! in stored order (pack, then offset), which puts every OFS base before
//! its deltas. A delta is re-pointed at its base's new position, sent as a
//! REF_DELTA when the base comes later or is one the client already has
//! (thin), and resolved into a whole object only when neither holds.
//! Stored bytes are read in windows of nearby entries, and an entry larger
//! than a window streams through without being held whole.
//!
//! The same generator rewrites a repository during garbage collection: the
//! send set is everything to keep, and `placed` records where each object
//! went, from which the new pack's index is built.

use super::pack::{self, OFS_DELTA, REF_DELTA};
use super::repo::{Index, PackInfo, NONE};
use super::walk::{Bits, Plan};
use super::{Kind, Oid};
use sha1::Digest;
use std::collections::HashMap;
use std::rc::Rc;

const GAP: u64 = 512 << 10;
const WINDOW: u64 = 8 << 20;
const HEAD_PEEK: u64 = 64;

pub trait PackIo {
    /// Plaintext bytes [start, end) of a stored pack.
    fn read(&mut self, pack: &PackInfo, start: u64, end: u64) -> Result<Vec<u8>, String>;
    /// A whole object, deltas resolved.
    fn object(&mut self, oid: &Oid) -> Result<(Kind, Vec<u8>), String>;
}

struct Item {
    idx: u32,
    oid: Oid,
    pack: Rc<PackInfo>,
    seq: u32,
    offset: u64,
    len: u64,
    stored: u8,
    base: u32,
    base_oid: Oid,
}

/// Where one object went in the generated pack.
#[derive(Clone, Copy, Debug)]
pub struct Placed {
    pub idx: u32,
    pub offset: u64,
    /// as written: a whole object's type, OFS_DELTA or REF_DELTA
    pub stored: u8,
    /// the delta base (an index into the source index), or NONE
    pub base: u32,
}

enum Stage {
    Header,
    Items,
    Trailer,
    Done,
}

struct Stream {
    item: usize,
    next: u64,
    end: u64,
}

pub struct Gen {
    items: Vec<Item>,
    pos: usize,
    in_send: Bits,
    has: Bits,
    emitted: HashMap<u32, u64>,
    written: u64,
    hasher: sha1::Sha1,
    ofs: bool,
    thin: bool,
    window: Option<(u32, u64, Vec<u8>)>,
    stream: Option<Stream>,
    stage: Stage,
    pub resolved: usize,
    pub bytes_read: u64,
    /// filled when recording (`record`)
    pub placed: Option<Vec<Placed>>,
}

impl Gen {
    pub fn new(ix: &Index, plan: Plan, ofs: bool, thin: bool) -> Gen {
        let mut in_send = Bits::new(ix.len());
        let mut items: Vec<Item> = plan
            .send
            .iter()
            .map(|&i| {
                in_send.set(i);
                let o = ix.obj(i);
                Item {
                    idx: i,
                    oid: ix.oid(i),
                    pack: ix.packs[o.pack as usize].clone(),
                    seq: o.pack,
                    offset: o.offset,
                    len: o.len,
                    stored: o.stored,
                    base: o.base,
                    base_oid: if o.base == NONE {
                        Oid::default()
                    } else {
                        ix.oid(o.base)
                    },
                }
            })
            .collect();
        items.sort_by_key(|it| (it.seq, it.offset));
        Gen {
            items,
            pos: 0,
            in_send,
            has: plan.has,
            emitted: HashMap::new(),
            written: 0,
            hasher: sha1::Sha1::new(),
            ofs,
            thin,
            window: None,
            stream: None,
            stage: Stage::Header,
            resolved: 0,
            bytes_read: 0,
            placed: None,
        }
    }

    /// Record where every object goes (see `placed`).
    pub fn record(mut self) -> Gen {
        self.placed = Some(Vec::with_capacity(self.items.len()));
        self
    }

    /// Bytes produced so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn total(&self) -> usize {
        self.items.len()
    }
    pub fn sent(&self) -> usize {
        self.pos
    }

    fn emit(&mut self, out: &mut Vec<u8>, b: &[u8]) {
        self.hasher.update(b);
        self.written += b.len() as u64;
        out.extend_from_slice(b);
    }

    /// Bytes of the current item from the window, fetching a new window
    /// (this item and its near neighbours) when it is not covered.
    fn bytes(&mut self, io: &mut dyn PackIo, start: u64, end: u64) -> Result<&[u8], String> {
        let it = &self.items[self.pos];
        let covered = matches!(&self.window, Some((seq, ws, w)) if *seq == it.seq && *ws <= start && ws + w.len() as u64 >= end);
        if !covered {
            let (ws, mut we) = (start, end);
            if end - start <= WINDOW {
                for next in &self.items[self.pos + 1..] {
                    if next.seq != it.seq
                        || next.offset > we + GAP
                        || next.offset + next.len - ws > WINDOW
                    {
                        break;
                    }
                    we = we.max(next.offset + next.len);
                }
            }
            let data = io.read(&it.pack, ws, we)?;
            if data.len() as u64 != we - ws {
                return Err("short read from stored pack".into());
            }
            self.bytes_read += data.len() as u64;
            self.window = Some((it.seq, ws, data));
        }
        let (_, ws, w) = self.window.as_ref().unwrap();
        Ok(&w[(start - ws) as usize..(end - ws) as usize])
    }

    /// Append up to roughly `budget` bytes of pack to `out`; Ok(true) once
    /// the trailer is out.
    pub fn produce(
        &mut self,
        io: &mut dyn PackIo,
        out: &mut Vec<u8>,
        budget: usize,
    ) -> Result<bool, String> {
        let stop = out.len() + budget;
        loop {
            match self.stage {
                Stage::Header => {
                    let mut h = Vec::with_capacity(12);
                    h.extend_from_slice(b"PACK");
                    h.extend_from_slice(&2u32.to_be_bytes());
                    h.extend_from_slice(&(self.items.len() as u32).to_be_bytes());
                    self.emit(out, &h);
                    self.stage = Stage::Items;
                }
                Stage::Items => {
                    if out.len() >= stop {
                        return Ok(false);
                    }
                    if let Some(s) = self.stream.take() {
                        let take = (s.end - s.next).min(WINDOW);
                        let chunk = {
                            let it = &self.items[s.item];
                            let pack = it.pack.clone();
                            io.read(&pack, s.next, s.next + take)?
                        };
                        if chunk.len() as u64 != take {
                            return Err("short read from stored pack".into());
                        }
                        self.bytes_read += take;
                        self.emit(out, &chunk);
                        if s.next + take < s.end {
                            self.stream = Some(Stream {
                                next: s.next + take,
                                ..s
                            });
                        } else {
                            self.pos += 1;
                        }
                        continue;
                    }
                    if self.pos == self.items.len() {
                        self.stage = Stage::Trailer;
                        continue;
                    }
                    self.item(io, out)?;
                }
                Stage::Trailer => {
                    let d: [u8; 20] = std::mem::take(&mut self.hasher).finalize().into();
                    out.extend_from_slice(&d);
                    self.written += 20;
                    self.window = None;
                    self.stage = Stage::Done;
                    return Ok(true);
                }
                Stage::Done => return Ok(true),
            }
        }
    }

    fn item(&mut self, io: &mut dyn PackIo, out: &mut Vec<u8>) -> Result<(), String> {
        let (offset, len, stored, base, base_oid, idx, oid) = {
            let it = &self.items[self.pos];
            (
                it.offset,
                it.len,
                it.stored,
                it.base,
                it.base_oid,
                it.idx,
                it.oid,
            )
        };
        let big = len > WINDOW;
        let head_end = if big {
            offset + HEAD_PEEK.min(len)
        } else {
            offset + len
        };
        let raw = self.bytes(io, offset, head_end)?.to_vec();
        let h = pack::parse_header(&raw, offset)?.ok_or("stored pack entry header truncated")?;
        if h.kind != stored {
            return Err("stored pack entry does not match its index".into());
        }
        let here = self.written;
        let mut head = Vec::with_capacity(32);
        let mut resolve = false;
        let mut placed = Placed {
            idx,
            offset: here,
            stored,
            base: NONE,
        };
        if stored == OFS_DELTA || stored == REF_DELTA {
            if let Some(&at) = self.emitted.get(&base).filter(|_| self.ofs) {
                pack::encode_header(OFS_DELTA, h.size, &mut head);
                pack::encode_ofs(here - at, &mut head);
                (placed.stored, placed.base) = (OFS_DELTA, base);
            } else if self.in_send.get(base) || (self.thin && self.has.get(base)) {
                pack::encode_header(REF_DELTA, h.size, &mut head);
                head.extend_from_slice(&base_oid.0);
                (placed.stored, placed.base) = (REF_DELTA, base);
            } else {
                resolve = true;
            }
        } else {
            head.extend_from_slice(&raw[..h.header_len]);
        }
        self.emitted.insert(idx, here);
        if resolve {
            let (kind, data) = io.object(&oid)?;
            placed.stored = kind as u8;
            if let Some(p) = &mut self.placed {
                p.push(placed);
            }
            let mut b = Vec::with_capacity(data.len() / 2 + 32);
            pack::encode_header(kind as u8, data.len() as u64, &mut b);
            b.extend_from_slice(&pack::deflate(&data));
            self.emit(out, &b);
            self.resolved += 1;
            self.pos += 1;
            return Ok(());
        }
        if let Some(p) = &mut self.placed {
            p.push(placed);
        }
        self.emit(out, &head);
        let data_start = offset + h.header_len as u64;
        if big {
            self.stream = Some(Stream {
                item: self.pos,
                next: data_start,
                end: offset + len,
            });
        } else {
            let body = raw[h.header_len..].to_vec();
            self.emit(out, &body);
            self.pos += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ingest::tests::{build_pack, naive_delta, NoBases, T};
    use crate::git::ingest::{Ingest, Limits};
    use std::time::{Duration, Instant};

    struct Mem(Vec<u8>, Index);
    impl PackIo for Mem {
        fn read(&mut self, _p: &PackInfo, s: u64, e: u64) -> Result<Vec<u8>, String> {
            Ok(self.0[s as usize..e as usize].to_vec())
        }
        fn object(&mut self, oid: &Oid) -> Result<(Kind, Vec<u8>), String> {
            // resolve through the pack itself
            let i = self.1.lookup(oid).ok_or("missing")?;
            let mut chain = vec![i];
            while self.1.obj(*chain.last().unwrap()).base != NONE {
                chain.push(self.1.obj(*chain.last().unwrap()).base);
            }
            let mut data: Option<Vec<u8>> = None;
            let mut kind = Kind::Blob;
            while let Some(j) = chain.pop() {
                let o = *self.1.obj(j);
                let raw = &self.0[o.offset as usize..(o.offset + o.len) as usize];
                let h = pack::parse_header(raw, o.offset).unwrap().unwrap();
                let (d, _) = pack::inflate_at(&raw[h.header_len..], h.size).unwrap();
                data = Some(match data {
                    None => {
                        kind = o.kind;
                        d
                    }
                    Some(b) => crate::git::delta::apply(&b, &d, 1 << 30).unwrap(),
                });
            }
            Ok((kind, data.unwrap()))
        }
    }

    fn index_of(pack: &[u8]) -> (Index, Vec<u8>) {
        let mut ing = Ingest::new(Limits {
            max_pack: 1 << 30,
            max_object: 1 << 28,
        });
        ing.feed(pack).unwrap();
        ing.finish_input().unwrap();
        while !ing
            .resolve(Instant::now() + Duration::from_secs(5), &mut NoBases)
            .unwrap()
        {}
        let (bytes, idx) = ing.complete().unwrap();
        let bytes = bytes.to_vec();
        let mut ix = Index::default();
        ix.add_pack(
            Rc::new(PackInfo {
                id: "p".into(),
                len: bytes.len() as u64,
                chunk: 262144,
            }),
            &idx,
        )
        .unwrap();
        (ix, bytes)
    }

    fn regen(
        ix: Index,
        bytes: Vec<u8>,
        send: &[u32],
        has: &[u32],
        ofs: bool,
        thin: bool,
    ) -> (Vec<u8>, usize) {
        let mut hb = Bits::new(ix.len());
        for &h in has {
            hb.set(h);
        }
        let p = Plan {
            send: send.to_vec(),
            has: hb,
            shallow: vec![],
            unshallow: vec![],
        };
        let mut g = Gen::new(&ix, p, ofs, thin);
        let mut io = Mem(bytes, ix);
        let mut out = Vec::new();
        while !g.produce(&mut io, &mut out, 7).unwrap() {}
        (out, g.resolved)
    }

    #[test]
    fn regenerated_packs_reindex_to_the_same_objects() {
        let a = b"alpha\n".repeat(50);
        let mut b = a.clone();
        b.extend_from_slice(b"beta\n");
        let mut c = b.clone();
        c.extend_from_slice(b"gamma\n");
        let pack = build_pack(&[
            T::Whole(Kind::Blob, &a),
            T::Ofs(0, naive_delta(&a, &b)),
            T::Ofs(1, naive_delta(&b, &c)),
        ]);
        let (ix, bytes) = index_of(&pack);
        let all: Vec<u32> = (0..3).collect();
        for (ofs, thin) in [(true, true), (false, false)] {
            let (ix2, bytes2) = index_of(&pack);
            let (out, resolved) = regen(ix2, bytes2, &all, &[], ofs, thin);
            assert_eq!(resolved, 0);
            let (rix, _) = index_of(&out);
            assert_eq!(rix.len(), 3);
            for i in 0..3 {
                assert!(rix.lookup(&ix.oid(i)).is_some());
            }
        }
        // only the newest object, base not sendable: thin -> REF delta (pack is thin)
        let (ix2, bytes2) = index_of(&pack);
        let (out, resolved) = regen(ix2, bytes2, &[2], &[1], true, true);
        assert_eq!(resolved, 0);
        let mut ing = Ingest::new(Limits {
            max_pack: 1 << 30,
            max_object: 1 << 28,
        });
        ing.feed(&out).unwrap();
        ing.finish_input().unwrap();
        assert!(
            ing.resolve(Instant::now() + Duration::from_secs(5), &mut NoBases)
                .is_err(),
            "thin"
        );
        // without thin-pack the object must arrive whole
        let (ix2, bytes2) = index_of(&pack);
        let (out, resolved) = regen(ix2, bytes2, &[2], &[1], true, false);
        assert_eq!(resolved, 1);
        let (rix, _) = index_of(&out);
        assert_eq!(rix.oid(0), ix.oid(2));
        drop(bytes);
    }
}
