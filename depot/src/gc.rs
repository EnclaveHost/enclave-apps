//! Garbage collection: rewrite a repository into one pack holding only what
//! is worth keeping, and retire every pack it had.
//!
//! Kept: everything the refs reach, plus everything a dropped ref value
//! (a force-pushed-away or deleted ref, see `Manifest::dropped`) reaches
//! while it is inside the retention window (`keep_days`). Dropped history
//! is never served to readers; it is kept so an admin can restore it.
//! Everything else (expired dropped history, duplicates, objects a push
//! brought that nothing references) is left out, and goes for good when the
//! retired packs are deleted an hour later.
//!
//! The new pack is produced by the fetch pack generator with the kept set
//! as its send set, so stored entries are copied as they are (deltas
//! re-pointed, and resolved only when their base is not kept), streamed
//! into storage without holding the pack whole. Resolved objects are
//! re-hashed before they are written.
//!
//! Pushes may land while a collection runs. Their packs are kept as they
//! are, and the commit (a compare-and-swap on the manifest) first checks
//! that everything the refs reach by then is in the new pack or in theirs;
//! a push that built on an object this collection leaves out fails its own
//! commit instead (it re-checks connectivity against the new index).

use crate::app::{now, read_obj, App, ObjCache, Repo};
use crate::git::ingest::IdxEntry;
use crate::git::packgen::{Gen, PackIo, Placed};
use crate::git::repo::{Index, PackInfo, NONE};
use crate::git::walk::{closure, Bits, Plan};
use crate::git::{object_id, Kind, Oid};
use crate::store::{PackMeta, Retired, Saved, Store, Upload, PART_BYTES};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

const SLICE: Duration = Duration::from_millis(30);
const PRODUCE: usize = 1 << 20;

/// What a collection would do to a repository now.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Garbage {
    /// objects it would leave out
    pub objects: usize,
    /// stored bytes it would free (about)
    pub bytes: u64,
    /// dropped ref values past the retention window
    pub expired: usize,
    /// dropped ref values still kept
    pub kept_drops: usize,
}

/// Dropped values at or before `cutoff` are expired.
fn cutoff(app: &App, purge: bool) -> u64 {
    let t = now();
    if purge {
        t
    } else {
        t.saturating_sub(app.cfg.keep)
    }
}

/// The ref values and live dropped values, as index positions.
fn tips(r: &Repo, cutoff: u64) -> Vec<u32> {
    r.refs
        .values()
        .copied()
        .chain(
            r.m.dropped
                .iter()
                .filter(|d| d.at > cutoff)
                .filter_map(|d| Oid::from_hex(&d.id)),
        )
        .filter_map(|o| r.ix.lookup(&o))
        .collect()
}

pub fn assess(r: &mut Repo, cutoff: u64) -> Garbage {
    let live = r.m.dropped.iter().filter(|d| d.at > cutoff).count();
    let key = (r.m.rev, r.ix.len(), live);
    let (kept, kept_bytes) = match &r.gc_cache {
        Some((k, v)) if *k == key => *v,
        _ => {
            let keep = closure(&r.ix, &tips(r, cutoff));
            let mut n = 0usize;
            let mut bytes = 0u64;
            for i in 0..r.ix.len() as u32 {
                if keep.get(i) {
                    n += 1;
                    bytes += r.ix.obj(i).len;
                }
            }
            r.gc_cache = Some((key, (n, bytes)));
            (n, bytes)
        }
    };
    let stored: u64 = r.m.packs.iter().map(|p| p.len.saturating_sub(32)).sum();
    Garbage {
        objects: r.ix.len() - kept,
        bytes: stored.saturating_sub(kept_bytes),
        expired: r.m.dropped.len() - live,
        kept_drops: live,
    }
}

/// Is a collection worth running? Expired dropped history always is (it
/// may be a secret someone force-pushed away); other garbage once it is
/// at least 16 MiB and a tenth of the repository.
pub fn due(g: &Garbage, repo_bytes: u64) -> bool {
    (g.expired > 0 && g.objects > 0) || (g.bytes >= 16 << 20 && g.bytes * 10 >= repo_bytes)
}

pub struct Collect {
    pub repo: String,
    pub new_id: String,
    pub purge: bool,
    sources: Vec<PackMeta>,
    /// the index's packs when planned: it must still begin with them
    snapshot: Vec<Rc<PackInfo>>,
    snap_len: usize,
    keep: Bits,
    pub kept: usize,
    pub dropped: usize,
    cutoff: u64,
    gen: Gen,
    produced: bool,
    hold: Option<Instant>,
    pub upload: Upload,
    pub started: Instant,
    pub before: u64,
}

impl Collect {
    pub fn progress(&self) -> (u64, u64) {
        (self.gen.written(), self.before)
    }
}

/// Plan a collection of repository `id`; None when there is nothing to do.
pub fn start(app: &mut App, id: &str, purge: bool) -> Result<Option<Collect>, String> {
    app.revalidate(id, true)?;
    let cut = cutoff(app, purge);
    let r = app.repos.get(id).ok_or("repository unloaded")?;
    if r.m.packs.is_empty() {
        return Ok(None);
    }
    let keep = closure(&r.ix, &tips(r, cut));
    let send: Vec<u32> = (0..r.ix.len() as u32).filter(|&i| keep.get(i)).collect();
    // the new index must name every link; refuse before writing anything
    if let Some(&i) = send.iter().find(|&&i| r.ix.kids(i).contains(&NONE)) {
        return Err(format!(
            "object {} links to an object the index does not hold; not collecting",
            r.ix.oid(i)
        ));
    }
    let expired = r.m.dropped.iter().filter(|d| d.at <= cut).count();
    if send.len() == r.ix.len() && r.m.packs.len() == 1 && expired == 0 {
        return Ok(None);
    }
    let has = Bits::new(r.ix.len());
    let plan = Plan {
        send,
        has,
        shallow: Vec::new(),
        unshallow: Vec::new(),
    };
    let kept = plan.send.len();
    let gen = Gen::new(&r.ix, plan, true, false).record();
    let new_id = crate::seal::random_id();
    eprintln!(
        "[depot] collect {}: keeping {kept} of {} objects ({} packs, {:.1} MiB){}",
        r.name,
        r.ix.len(),
        r.m.packs.len(),
        r.bytes() as f64 / 1048576.0,
        if purge { ", purging dropped history" } else { "" }
    );
    let c = Collect {
        repo: id.to_string(),
        new_id: new_id.clone(),
        purge,
        sources: r.m.packs.clone(),
        snapshot: r.ix.packs.clone(),
        snap_len: r.ix.len(),
        dropped: r.ix.len() - kept,
        keep,
        kept,
        cutoff: cut,
        gen,
        produced: false,
        hold: None,
        upload: app.store.upload_begin(id, &new_id),
        started: Instant::now(),
        before: r.bytes(),
    };
    Ok(Some(c))
}

/// The index still extends the one the collection planned against (the
/// same packs first, so the same positions): nothing rebuilt it.
fn extends(ix: &Index, c: &Collect) -> bool {
    ix.len() >= c.snap_len
        && ix.packs.len() >= c.snapshot.len()
        && ix.packs.iter().zip(&c.snapshot).all(|(a, b)| Rc::ptr_eq(a, b))
}

/// Pack IO that reads around the chunk cache (a whole repository must not
/// evict what fetches use) and re-hashes every object it resolves.
struct GcIo<'a> {
    store: &'a mut Store,
    repo: &'a str,
    ix: &'a Index,
    cache: &'a mut ObjCache,
    max: u64,
}

impl PackIo for GcIo<'_> {
    fn read(&mut self, p: &PackInfo, start: u64, end: u64) -> Result<Vec<u8>, String> {
        self.store.read_pack_uncached(self.repo, p, start, end)
    }
    fn object(&mut self, oid: &Oid) -> Result<(Kind, Vec<u8>), String> {
        let i = self.ix.lookup(oid).ok_or("object vanished from the index")?;
        let (k, d) = read_obj(self.store, self.repo, self.ix, self.cache, i, self.max)?;
        if object_id(k, &d)? != *oid {
            return Err(format!("stored object {oid} does not hash to its id"));
        }
        Ok((k, d.as_ref().clone()))
    }
}

/// One slice of a collection; Ok(true) once committed.
pub fn step(app: &mut App, c: &mut Collect) -> Result<bool, String> {
    let t0 = Instant::now();
    while t0.elapsed() < SLICE {
        if app.store.upload_step(&mut c.upload)? {
            continue;
        }
        if !c.produced {
            let App {
                store,
                repos,
                objects,
                cfg,
                ..
            } = app;
            let r = repos.get(&c.repo).ok_or("repository unloaded")?;
            if !extends(&r.ix, c) {
                return Err("the repository was repacked meanwhile".into());
            }
            let mut io = GcIo {
                store,
                repo: &c.repo,
                ix: &r.ix,
                cache: objects,
                max: cfg.max_object,
            };
            let mut out = Vec::with_capacity(PRODUCE + (64 << 10));
            // keep at most a couple of parts waiting
            while c.upload.pending() as u64 <= 2 * PART_BYTES && !c.produced {
                c.produced = c.gen.produce(&mut io, &mut out, PRODUCE)?;
                c.upload.write(&out);
                out.clear();
                if t0.elapsed() >= SLICE {
                    break;
                }
            }
            continue;
        }
        let total = app.store.upload_finish(&mut c.upload)?;
        if total != c.gen.written() {
            return Err("internal: stored pack length differs".into());
        }
        // tests widen the window in which pushes race the commit
        let hold = std::env::var("DEPOT_COLLECT_HOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs);
        if let Some(h) = hold {
            let until = *c.hold.get_or_insert_with(|| Instant::now() + h);
            if Instant::now() < until {
                return Ok(false);
            }
        }
        let r = app.repos.get(&c.repo).ok_or("repository unloaded")?;
        if !extends(&r.ix, c) {
            return Err("the repository was repacked meanwhile".into());
        }
        let placed = c.gen.placed.take().unwrap_or_default();
        let entries = entries(&r.ix, &placed, total)?;
        app.store.put_idx(&c.repo, &c.new_id, &entries)?;
        commit(app, c, entries.len() as u32, total)?;
        return Ok(true);
    }
    Ok(false)
}

/// The new pack's index, from where each object went and what the
/// repository's index knows about it.
fn entries(ix: &Index, placed: &[Placed], total: u64) -> Result<Vec<IdxEntry>, String> {
    let mut out = Vec::with_capacity(placed.len());
    for (n, p) in placed.iter().enumerate() {
        let end = placed.get(n + 1).map_or(total - 20, |q| q.offset);
        let o = ix.obj(p.idx);
        let mut children = Vec::with_capacity(o.nkids as usize);
        for &k in ix.kids(p.idx) {
            if k == NONE {
                return Err(format!(
                    "object {} links to an object the index does not hold; not collecting",
                    ix.oid(p.idx)
                ));
            }
            children.push((ix.obj(k).kind, ix.oid(k)));
        }
        out.push(IdxEntry {
            oid: ix.oid(p.idx),
            offset: p.offset,
            len: end - p.offset,
            stored: p.stored,
            base: (p.base != NONE).then(|| ix.oid(p.base)),
            kind: o.kind,
            size: o.size,
            time: o.time,
            children,
        });
    }
    Ok(out)
}

fn commit(app: &mut App, c: &Collect, objects: u32, total: u64) -> Result<(), String> {
    let n = c.sources.len();
    for attempt in 0..8 {
        if attempt > 0 {
            app.revalidate(&c.repo, true)?;
        }
        let r = app.repos.get(&c.repo).ok_or("repository unloaded")?;
        if r.m.lists(&c.new_id) {
            return Ok(()); // an earlier attempt landed
        }
        // pushes only append packs; anything else changed the repository
        // under us (a repack or collection elsewhere)
        if r.m.packs.len() < n || r.m.packs[..n] != c.sources[..] {
            return Err("the repository's packs changed during the collection".into());
        }
        if !extends(&r.ix, c) {
            return Err("the repository was re-indexed during the collection".into());
        }
        let mut m = r.m.clone();
        m.dropped.retain(|d| d.at > c.cutoff);
        // everything the refs (and the dropped values still kept) reach by
        // now must survive: in the new pack, or in a pack pushed meanwhile
        let tips: Vec<u32> = r
            .refs
            .values()
            .copied()
            .chain(m.dropped.iter().filter_map(|d| Oid::from_hex(&d.id)))
            .filter_map(|o| r.ix.lookup(&o))
            .collect();
        let need = closure(&r.ix, &tips);
        let mut later: Option<HashSet<Oid>> = None;
        for i in 0..c.snap_len as u32 {
            if !need.get(i) || c.keep.get(i) {
                continue;
            }
            if later.is_none() {
                let mut s = HashSet::new();
                for p in &r.m.packs[n..] {
                    s.extend(app.store.get_idx(&c.repo, &p.id)?.into_iter().map(|e| e.oid));
                }
                later = Some(s);
            }
            if !later.as_ref().unwrap().contains(&r.ix.oid(i)) {
                return Err(format!(
                    "a push during the collection needs {}, which it leaves out; will retry",
                    r.ix.oid(i)
                ));
            }
        }
        let merged = PackMeta {
            id: c.new_id.clone(),
            len: total,
            objects,
            chunk: crate::seal::CHUNK,
        };
        m.packs.splice(..n, [merged]);
        let at = now();
        m.retired.extend(c.sources.iter().map(|p| Retired {
            id: p.id.clone(),
            at,
        }));
        let etag = r.etag.clone();
        match app.store.save_manifest(&c.repo, &mut m, etag.as_deref(), at)? {
            Saved::Ok(t) => {
                if let Err(e) = app.apply(&c.repo, m, t) {
                    eprintln!("[depot] collection committed; re-indexing deferred: {e}");
                    app.mark_stale(&c.repo);
                }
                return Ok(());
            }
            Saved::Conflict => continue,
        }
    }
    Err("manifest busy".into())
}

/// Forget expired dropped values when no object would be freed by a
/// collection (the history they named is still reachable otherwise).
pub fn prune(app: &mut App, id: &str) -> Result<(), String> {
    let cut = cutoff(app, false);
    for attempt in 0..8 {
        if attempt > 0 {
            app.revalidate(id, true)?;
        }
        let r = app.repos.get(id).ok_or("repository unloaded")?;
        if !r.m.dropped.iter().any(|d| d.at <= cut) {
            return Ok(());
        }
        let mut m = r.m.clone();
        m.dropped.retain(|d| d.at > cut);
        let etag = r.etag.clone();
        match app.store.save_manifest(id, &mut m, etag.as_deref(), now())? {
            Saved::Ok(t) => {
                let r = app.repos.get_mut(id).unwrap();
                r.m = m;
                r.etag = Some(t);
                return Ok(());
            }
            Saved::Conflict => continue,
        }
    }
    Err("manifest busy".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::ingest::tests::{build_pack, naive_delta, NoBases, T};
    use crate::git::ingest::{Ingest, Limits};
    use crate::git::pack;

    fn ingest(p: &[u8]) -> (Vec<u8>, Vec<IdxEntry>) {
        let mut ing = Ingest::new(Limits {
            max_pack: 1 << 30,
            max_object: 1 << 28,
        });
        ing.feed(p).unwrap();
        ing.finish_input().unwrap();
        while !ing
            .resolve(Instant::now() + Duration::from_secs(5), &mut NoBases)
            .unwrap()
        {}
        let (b, e) = ing.complete().unwrap();
        (b.to_vec(), e)
    }

    /// Pack IO over one in-memory pack, resolving through its index.
    struct Mem<'a>(&'a [u8], &'a Index);
    impl PackIo for Mem<'_> {
        fn read(&mut self, _: &PackInfo, s: u64, e: u64) -> Result<Vec<u8>, String> {
            Ok(self.0[s as usize..e as usize].to_vec())
        }
        fn object(&mut self, oid: &Oid) -> Result<(Kind, Vec<u8>), String> {
            let mut chain = vec![self.1.lookup(oid).unwrap()];
            while self.1.obj(*chain.last().unwrap()).base != NONE {
                chain.push(self.1.obj(*chain.last().unwrap()).base);
            }
            let mut data: Option<Vec<u8>> = None;
            while let Some(j) = chain.pop() {
                let o = *self.1.obj(j);
                let raw = &self.0[o.offset as usize..(o.offset + o.len) as usize];
                let h = pack::parse_header(raw, o.offset).unwrap().unwrap();
                let (d, _) = pack::inflate_at(&raw[h.header_len..], h.size).unwrap();
                data = Some(match data {
                    None => d,
                    Some(b) => crate::git::delta::apply(&b, &d, 1 << 30).unwrap(),
                });
            }
            Ok((self.1.obj(self.1.lookup(oid).unwrap()).kind, data.unwrap()))
        }
    }

    fn tree(entries: &[(&str, Oid)]) -> Vec<u8> {
        let mut t = Vec::new();
        for (name, o) in entries {
            t.extend_from_slice(format!("100644 {name}\0").as_bytes());
            t.extend_from_slice(&o.0);
        }
        t
    }

    #[test]
    fn collection_keeps_reachable_objects_and_its_index_matches_index_pack() {
        let a = b"alpha\n".repeat(60);
        let mut b = a.clone();
        b.extend_from_slice(b"beta\n");
        let mut c = b.clone();
        c.extend_from_slice(b"gamma\n");
        let junk = b"nothing links here\n".repeat(9);
        let (ob, oc) = (
            object_id(Kind::Blob, &b).unwrap(),
            object_id(Kind::Blob, &c).unwrap(),
        );
        let t = tree(&[("b", ob), ("c", oc)]);
        let ot = object_id(Kind::Tree, &t).unwrap();
        let commit = format!(
            "tree {}\nauthor A <a@x> 1700000000 +0000\ncommitter A <a@x> 1700000000 +0000\n\nkeep\n",
            ot.hex()
        );
        // a is only a delta base: it goes, so b must be stored whole
        let p = build_pack(&[
            T::Whole(Kind::Blob, &a),
            T::Ofs(0, naive_delta(&a, &b)),
            T::Ofs(1, naive_delta(&b, &c)),
            T::Whole(Kind::Blob, &junk),
            T::Whole(Kind::Tree, &t),
            T::Whole(Kind::Commit, commit.as_bytes()),
        ]);
        let (bytes, idx) = ingest(&p);
        let mut ix = Index::default();
        let info = Rc::new(PackInfo {
            id: "p".into(),
            len: bytes.len() as u64,
            chunk: 1 << 18,
        });
        ix.add_pack(info, &idx).unwrap();
        let tip = ix.lookup(&object_id(Kind::Commit, commit.as_bytes()).unwrap()).unwrap();
        let keep = closure(&ix, &[tip]);
        let send: Vec<u32> = (0..ix.len() as u32).filter(|&i| keep.get(i)).collect();
        assert_eq!(send.len(), 4, "commit, tree, b, c");
        let plan = Plan {
            send,
            has: Bits::new(ix.len()),
            shallow: vec![],
            unshallow: vec![],
        };
        let mut g = Gen::new(&ix, plan, true, false).record();
        let mut out = Vec::new();
        let mut io = Mem(&bytes, &ix);
        while !g.produce(&mut io, &mut out, 50).unwrap() {}
        assert_eq!(g.resolved, 1, "b is resolved, c stays a delta on it");
        let ours = entries(&ix, g.placed.as_ref().unwrap(), out.len() as u64).unwrap();
        let (_, theirs) = ingest(&out);
        let key = |e: &IdxEntry| {
            (e.oid, e.offset, e.len, e.stored, e.base, e.kind as u8, e.size, e.time, e.children.clone())
        };
        let mut a1: Vec<_> = ours.iter().map(key).collect();
        let mut b1: Vec<_> = theirs.iter().map(key).collect();
        a1.sort_by_key(|k| k.1);
        b1.sort_by_key(|k| k.1);
        assert_eq!(a1, b1, "the collected pack's index is what index-pack makes of it");
        assert!(ours.iter().all(|e| e.oid != object_id(Kind::Blob, &junk).unwrap()));
        assert!(ours.iter().any(|e| e.oid == oc && e.stored == pack::OFS_DELTA));
    }

    #[test]
    fn due_thresholds() {
        let g = |objects, bytes, expired| Garbage {
            objects,
            bytes,
            expired,
            kept_drops: 0,
        };
        assert!(!due(&g(0, 0, 0), 100 << 20));
        assert!(due(&g(1, 10, 1), 100 << 20), "expired history goes regardless of size");
        assert!(!due(&g(0, 0, 3), 100 << 20), "expired but still reachable: prune only");
        assert!(!due(&g(9, 8 << 20, 0), 100 << 20), "small garbage waits");
        assert!(due(&g(9, 20 << 20, 0), 100 << 20));
        assert!(!due(&g(9, 20 << 20, 0), 1 << 30), "under a tenth waits");
    }
}
