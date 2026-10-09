//! Server state: the registry, the repositories loaded so far (manifest,
//! refs, object index), and the paths between them and storage.
//!
//! A repository is loaded on first use (its manifest, then every pack's
//! index) and revalidated with a conditional GET before each ref
//! advertisement, so a second server sharing the bucket is seen within a
//! couple of seconds. A manifest whose revision goes backwards is refused:
//! that is storage serving an older copy.

use crate::config::Config;
use crate::git::pack;
use crate::git::repo::{Index, PackInfo, NONE};
use crate::git::{delta, Kind, Oid};
use crate::store::{Loaded, Manifest, Registry, RepoMeta, Store};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

const REVALIDATE: Duration = Duration::from_secs(2);
const REGISTRY_MISS_REFRESH: Duration = Duration::from_secs(3);

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Repo {
    /// the index may not match the manifest (a re-index failed): rebuild it
    pub stale: bool,
    /// what readers may fetch, for one (revision, index size)
    pub reach_cache: Option<((u64, usize), Rc<crate::git::walk::Bits>)>,
    pub name: String,
    pub m: Manifest,
    pub etag: Option<String>,
    pub ix: Index,
    pub refs: BTreeMap<String, Oid>,
    pub checked: Instant,
}

impl Repo {
    pub fn view<'a>(&'a self, reach: &'a crate::git::walk::Bits) -> crate::git::upload::View<'a> {
        crate::git::upload::View {
            ix: &self.ix,
            refs: &self.refs,
            head: &self.m.head,
            reach,
        }
    }

    /// Every object reachable from the refs, computed once per revision.
    pub fn reach(&mut self) -> Rc<crate::git::walk::Bits> {
        let key = (self.m.rev, self.ix.len());
        if let Some((k, b)) = &self.reach_cache {
            if *k == key {
                return b.clone();
            }
        }
        let tips: Vec<u32> = self
            .refs
            .values()
            .filter_map(|o| self.ix.lookup(o))
            .collect();
        let b = Rc::new(crate::git::walk::closure(&self.ix, &tips));
        self.reach_cache = Some((key, b.clone()));
        b
    }
    pub fn bytes(&self) -> u64 {
        self.m.packs.iter().map(|p| p.len).sum()
    }
}

#[derive(Default)]
pub struct Stats {
    pub fetches: u64,
    pub pushes: u64,
    pub push_bytes: u64,
    pub sent_bytes: u64,
    pub failures: u64,
}

/// Resolved objects (for thin-pack bases and the web view), LRU by bytes.
pub struct ObjCache {
    map: HashMap<Oid, (Kind, Rc<Vec<u8>>, u64)>,
    order: VecDeque<(Oid, u64)>,
    tick: u64,
    bytes: usize,
    budget: usize,
}

impl ObjCache {
    pub fn new(budget: usize) -> ObjCache {
        ObjCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            tick: 0,
            bytes: 0,
            budget,
        }
    }
    fn get(&mut self, o: &Oid) -> Option<(Kind, Rc<Vec<u8>>)> {
        let tick = self.tick + 1;
        let e = self.map.get_mut(o)?;
        self.tick = tick;
        e.2 = tick;
        self.order.push_back((*o, tick));
        Some((e.0, e.1.clone()))
    }
    fn put(&mut self, o: Oid, k: Kind, v: Rc<Vec<u8>>) {
        if v.len() > self.budget / 8 {
            return;
        }
        self.tick += 1;
        self.bytes += v.len();
        if let Some((_, old, _)) = self.map.insert(o, (k, v, self.tick)) {
            self.bytes -= old.len();
        }
        self.order.push_back((o, self.tick));
        while self.bytes > self.budget {
            let Some((ok, t)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&ok).map(|e| e.2) == Some(t) {
                let (_, v, _) = self.map.remove(&ok).unwrap();
                self.bytes -= v.len();
            }
        }
        if self.order.len() > 4 * self.map.len() + 1024 {
            let map = &self.map;
            self.order
                .retain(|(k, t)| map.get(k).map(|e| e.2) == Some(*t));
        }
    }
}

pub struct App {
    pub cfg: Config,
    pub store: Store,
    pub reg: Registry,
    pub reg_etag: Option<String>,
    reg_checked: Option<Instant>,
    pub repos: HashMap<String, Repo>,
    pub objects: ObjCache,
    pub started: Instant,
    pub stats: Stats,
    pub maint: crate::maint::State,
    pub tokens: crate::tokens::Tokens,
    pub push_budget: crate::push::Budget,
    pub raw_budget: crate::push::Budget,
    held: HashMap<String, std::rc::Weak<PackInfo>>,
    pub hooks: crate::hooks::Queue,
}

pub fn parse_refs(m: &Manifest) -> Result<BTreeMap<String, Oid>, String> {
    m.refs
        .iter()
        .map(|(k, v)| {
            Oid::from_hex(v)
                .map(|o| (k.clone(), o))
                .ok_or_else(|| format!("manifest ref {k} is not an object id"))
        })
        .collect()
}

fn pack_info(p: &crate::store::PackMeta) -> Rc<PackInfo> {
    Rc::new(PackInfo {
        id: p.id.clone(),
        len: p.len,
        chunk: p.chunk,
    })
}

impl App {
    pub fn new(cfg: Config, store: Store) -> App {
        let budget = (cfg.cache_mb / 4).max(16) << 20;
        App {
            cfg,
            store,
            reg: Registry::default(),
            reg_etag: None,
            reg_checked: None,
            repos: HashMap::new(),
            objects: ObjCache::new(budget),
            started: Instant::now(),
            stats: Stats::default(),
            maint: Default::default(),
            tokens: crate::tokens::Tokens::new(),
            push_budget: Default::default(),
            raw_budget: Default::default(),
            held: HashMap::new(),
            hooks: Default::default(),
        }
    }

    pub fn refresh_registry(&mut self, force: bool) -> Result<(), String> {
        if !force
            && self
                .reg_checked
                .is_some_and(|t| t.elapsed() < REGISTRY_MISS_REFRESH)
        {
            return Ok(());
        }
        match self.store.load_registry(self.reg_etag.as_deref())? {
            Loaded::NotModified => {}
            Loaded::Missing => {
                if self.reg_etag.is_some() {
                    return Err("the registry disappeared from storage".into());
                }
            }
            Loaded::Found(r, t) => {
                if r.rev < self.reg.rev {
                    return Err(
                        "storage served an older registry than this server has seen (rollback?)"
                            .into(),
                    );
                }
                // repositories deleted elsewhere drop out of memory
                let ids: HashSet<&String> = r.repos.values().map(|m| &m.id).collect();
                self.repos.retain(|id, _| ids.contains(id));
                self.reg = r;
                self.reg_etag = Some(t);
            }
        }
        self.reg_checked = Some(Instant::now());
        Ok(())
    }

    pub fn meta(&mut self, name: &str) -> Result<Option<RepoMeta>, String> {
        if let Some(m) = self.reg.repos.get(name) {
            return Ok(Some(m.clone()));
        }
        self.refresh_registry(false)?;
        Ok(self.reg.repos.get(name).cloned())
    }

    pub fn is_public(&self, name: &str) -> bool {
        self.reg.repos.get(name).is_some_and(|m| m.public) || self.cfg.public_by_config(name)
    }

    /// Load (or revalidate) a repository by name; Ok(None) when it does not exist.
    pub fn open(&mut self, name: &str) -> Result<Option<String>, String> {
        let Some(meta) = self.meta(name)? else {
            return Ok(None);
        };
        if self.repos.contains_key(&meta.id) {
            self.revalidate(&meta.id, false)?;
        } else {
            self.load(&meta.id, name)?;
        }
        Ok(Some(meta.id))
    }

    fn build_index(&mut self, id: &str, m: &Manifest) -> Result<Index, String> {
        let mut ix = Index::default();
        for p in &m.packs {
            let entries = self.store.get_idx(id, &p.id)?;
            ix.add_pack(pack_info(p), &entries)?;
        }
        if ix.dangling > 0 {
            eprintln!(
                "[depot] repository {id}: {} links to objects the index does not hold",
                ix.dangling
            );
        }
        Ok(ix)
    }

    fn load(&mut self, id: &str, name: &str) -> Result<(), String> {
        let (m, etag) = match self.store.load_manifest(id, None)? {
            Loaded::Found(m, t) => (m, Some(t)),
            _ => (
                Manifest::new(&format!("refs/heads/{}", self.cfg.default_branch)),
                None,
            ),
        };
        let t0 = Instant::now();
        let ix = self.build_index(id, &m)?;
        let refs = parse_refs(&m)?;
        if !m.packs.is_empty() {
            eprintln!(
                "[depot] loaded {name}: {} refs, {} objects in {} packs, {:.1}s",
                refs.len(),
                ix.len(),
                m.packs.len(),
                t0.elapsed().as_secs_f64()
            );
        }
        self.repos.insert(
            id.to_string(),
            Repo {
                stale: false,
                reach_cache: None,
                name: name.to_string(),
                m,
                etag,
                ix,
                refs,
                checked: Instant::now(),
            },
        );
        Ok(())
    }

    /// The index no longer matches what storage holds: rebuild it on the
    /// next look (the manifest is read whole, not conditionally).
    pub fn mark_stale(&mut self, id: &str) {
        if let Some(r) = self.repos.get_mut(id) {
            r.stale = true;
            r.etag = None;
        }
    }

    /// Is this pack still read by a fetch in flight (it holds the record)?
    pub fn held(&mut self, pack: &str) -> bool {
        self.held.retain(|_, w| w.strong_count() > 0);
        self.held.contains_key(pack)
    }

    pub fn revalidate(&mut self, id: &str, force: bool) -> Result<(), String> {
        let Some(r) = self.repos.get(id) else {
            return Err("repository not loaded".into());
        };
        if !force && !r.stale && r.checked.elapsed() < REVALIDATE {
            return Ok(());
        }
        let etag = r.etag.clone();
        match self.store.load_manifest(id, etag.as_deref())? {
            Loaded::NotModified => {}
            Loaded::Missing => {
                if etag.is_some() {
                    return Err("this repository's manifest disappeared from storage".into());
                }
            }
            Loaded::Found(m, t) => self.apply(id, m, t)?,
        }
        if let Some(r) = self.repos.get_mut(id) {
            r.checked = Instant::now();
        }
        Ok(())
    }

    /// Adopt a newer manifest: index the packs it adds, or rebuild when
    /// packs went away (a repack).
    pub fn apply(&mut self, id: &str, m: Manifest, etag: String) -> Result<(), String> {
        let r = self.repos.get(id).ok_or("repository not loaded")?;
        if m.rev < r.m.rev {
            return Err(
                "storage served an older manifest than this server has seen (rollback?)".into(),
            );
        }
        let old: Vec<&str> = r.m.packs.iter().map(|p| p.id.as_str()).collect();
        let prefix_kept =
            m.packs.len() >= old.len() && m.packs.iter().zip(&old).all(|(a, b)| a.id == *b);
        let refs = parse_refs(&m)?;
        if prefix_kept && !r.stale {
            // read every new index first: a storage failure changes nothing
            let mut add = Vec::new();
            for p in &m.packs[old.len()..] {
                add.push((pack_info(p), self.store.get_idx(id, &p.id)?));
            }
            let r = self.repos.get_mut(id).unwrap();
            for (info, entries) in add {
                if let Err(e) = r.ix.add_pack(info, &entries) {
                    r.stale = true;
                    return Err(e);
                }
            }
        } else {
            let ix = self.build_index(id, &m)?;
            let r = self.repos.get_mut(id).unwrap();
            // packs leaving the index may still be read by fetches in flight
            for p in &r.ix.packs {
                if !m.packs.iter().any(|x| x.id == p.id) {
                    self.held.insert(p.id.clone(), Rc::downgrade(p));
                }
            }
            r.ix = ix;
            r.stale = false;
        }
        let r = self.repos.get_mut(id).unwrap();
        r.m = m;
        r.etag = Some(etag);
        r.refs = refs;
        r.checked = Instant::now();
        Ok(())
    }

    /// A whole object with its deltas resolved.
    pub fn read_object(
        &mut self,
        id: &str,
        oid: &Oid,
    ) -> Result<Option<(Kind, Rc<Vec<u8>>)>, String> {
        let App {
            store,
            repos,
            objects,
            cfg,
            ..
        } = self;
        let r = repos.get(id).ok_or("repository not loaded")?;
        let Some(i) = r.ix.lookup(oid) else {
            return Ok(None);
        };
        read_obj(store, id, &r.ix, objects, i, cfg.max_object).map(Some)
    }
}

/// Resolve object `i` of `ix` by reading its delta chain from storage.
pub fn read_obj(
    store: &mut Store,
    repo: &str,
    ix: &Index,
    cache: &mut ObjCache,
    i: u32,
    max: u64,
) -> Result<(Kind, Rc<Vec<u8>>), String> {
    let kind = ix.obj(i).kind;
    let mut chain = Vec::new();
    let mut cur = i;
    let mut data: Option<Rc<Vec<u8>>> = None;
    loop {
        if let Some((_, c)) = cache.get(&ix.oid(cur)) {
            data = Some(c);
            break;
        }
        chain.push(cur);
        let o = ix.obj(cur);
        if o.base == NONE {
            break;
        }
        cur = o.base;
        if chain.len() > 10_000 {
            return Err("delta chain too deep".into());
        }
    }
    while let Some(j) = chain.pop() {
        let o = *ix.obj(j);
        let raw = store.read_pack(repo, &ix.packs[o.pack as usize], o.offset, o.offset + o.len)?;
        let h = pack::parse_header(&raw, o.offset)?.ok_or("stored entry header truncated")?;
        let (d, _) = pack::inflate_at(&raw[h.header_len..], h.size)?;
        let next = match data {
            None => Rc::new(d),
            Some(b) => Rc::new(delta::apply(&b, &d, max)?),
        };
        cache.put(ix.oid(j), kind, next.clone());
        data = Some(next);
    }
    Ok((kind, data.ok_or("empty delta chain")?))
}

/// Pack IO for fetch streams and thin-pack bases, bound to one repository.
pub struct RepoIo<'a> {
    pub store: &'a mut Store,
    pub repo: &'a str,
    pub ix: &'a Index,
    pub cache: &'a mut ObjCache,
    pub max: u64,
}

impl crate::git::packgen::PackIo for RepoIo<'_> {
    fn read(&mut self, p: &PackInfo, start: u64, end: u64) -> Result<Vec<u8>, String> {
        self.store.read_pack(self.repo, p, start, end)
    }
    fn object(&mut self, oid: &Oid) -> Result<(Kind, Vec<u8>), String> {
        let i = self
            .ix
            .lookup(oid)
            .ok_or("object vanished from the index")?;
        let (k, d) = read_obj(self.store, self.repo, self.ix, self.cache, i, self.max)?;
        Ok((k, d.as_ref().clone()))
    }
}

impl crate::git::ingest::Bases for RepoIo<'_> {
    fn base(&mut self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>, String> {
        let Some(i) = self.ix.lookup(oid) else {
            return Ok(None);
        };
        let (k, d) = read_obj(self.store, self.repo, self.ix, self.cache, i, self.max)?;
        Ok(Some((k, d.as_ref().clone())))
    }
}
