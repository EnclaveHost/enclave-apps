//! The bucket layout and everything that crosses into it.
//!
//! ```text
//! <prefix>registry               sealed: repository names -> random ids, settings
//! <prefix>r/<id>/manifest        sealed: HEAD, refs, the pack list (CAS on ETag)
//! <prefix>r/<id>/<pack>.pack     a git pack, sealed in 256 KiB chunks
//! <prefix>r/<id>/<pack>.idx      sealed: that pack's object index and links
//! ```
//!
//! Object keys carry only random ids, so the bucket shows no names. Packs
//! are immutable once written; the manifest is the only object rewritten,
//! always with If-Match on the ETag the writer read (If-None-Match for the
//! first write), so two servers sharing a bucket cannot lose each other's
//! updates. Pack reads go through an LRU of decrypted chunks.

use crate::git::ingest::IdxEntry;
use crate::git::repo::{self, PackInfo};
use crate::s3::{Cond, S3};
use crate::seal::{self, Chunked, Keys, CHUNK, TAG};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

/// Chunks per multipart part: 8 MiB of plaintext, every part the same size
/// (R2 requires that of all but the last).
const PART_CHUNKS: u64 = 32;
/// The most chunks one range GET fetches.
const FETCH_CHUNKS: u64 = 64;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Registry {
    pub v: u32,
    pub rev: u64,
    pub repos: BTreeMap<String, RepoMeta>,
    #[serde(default)]
    pub writer: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RepoMeta {
    pub id: String,
    pub created: u64,
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub description: String,
}

fn default_chunk() -> u32 {
    CHUNK
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PackMeta {
    pub id: String,
    pub len: u64,
    pub objects: u32,
    #[serde(default = "default_chunk")]
    pub chunk: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Retired {
    pub id: String,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub v: u32,
    pub rev: u64,
    pub head: String,
    pub refs: BTreeMap<String, String>,
    pub packs: Vec<PackMeta>,
    #[serde(default)]
    pub retired: Vec<Retired>,
    #[serde(default)]
    pub writer: String,
    #[serde(default)]
    pub updated: u64,
}

impl Manifest {
    pub fn new(head: &str) -> Manifest {
        Manifest {
            v: 1,
            rev: 0,
            head: head.to_string(),
            refs: BTreeMap::new(),
            packs: Vec::new(),
            retired: Vec::new(),
            writer: String::new(),
            updated: 0,
        }
    }
}

pub enum Loaded<T> {
    Missing,
    NotModified,
    Found(T, String),
}

pub enum Saved {
    Ok(String),
    Conflict,
}

struct ChunkCache {
    map: HashMap<(u64, u64), (Rc<Vec<u8>>, u64)>,
    order: VecDeque<((u64, u64), u64)>,
    tick: u64,
    bytes: usize,
    budget: usize,
    pub hits: u64,
    pub misses: u64,
}

impl ChunkCache {
    fn get(&mut self, k: (u64, u64)) -> Option<Rc<Vec<u8>>> {
        let tick = self.tick + 1;
        let e = self.map.get_mut(&k)?;
        self.tick = tick;
        e.1 = tick;
        self.order.push_back((k, tick));
        self.hits += 1;
        Some(e.0.clone())
    }
    fn put(&mut self, k: (u64, u64), v: Rc<Vec<u8>>) {
        self.tick += 1;
        self.bytes += v.len();
        if let Some((old, _)) = self.map.insert(k, (v, self.tick)) {
            self.bytes -= old.len();
        }
        self.order.push_back((k, self.tick));
        while self.bytes > self.budget {
            let Some((ok, t)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&ok).map(|e| e.1) == Some(t) {
                let (v, _) = self.map.remove(&ok).unwrap();
                self.bytes -= v.len();
            }
        }
        if self.order.len() > 4 * self.map.len() + 4096 {
            let map = &self.map;
            self.order
                .retain(|(k, t)| map.get(k).map(|e| e.1) == Some(*t));
        }
    }
}

pub struct Store {
    pub s3: S3,
    keys: Keys,
    prefix: String,
    cache: ChunkCache,
    writer: String,
}

/// An in-flight pack upload; `Store::upload_step` sends one part per call.
pub struct Upload {
    key: String,
    chunked: Chunked,
    upload_id: Option<String>,
    next: u64,
    parts: Vec<String>,
    pub sent: u64,
}

fn compress(b: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(b, 6)
}

fn decompress(b: &[u8]) -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(b, 1 << 30)
        .map_err(|_| "stored object does not decompress".to_string())
}

fn pack_tag(id: &str) -> u64 {
    u64::from_str_radix(id.get(..16).unwrap_or("0"), 16).unwrap_or(0)
}

impl Store {
    pub fn new(s3: S3, master: &str, prefix: &str, cache_mb: usize) -> Store {
        Store {
            s3,
            keys: Keys::new(master),
            prefix: prefix.to_string(),
            cache: ChunkCache {
                map: HashMap::new(),
                order: VecDeque::new(),
                tick: 0,
                bytes: 0,
                budget: cache_mb << 20,
                hits: 0,
                misses: 0,
            },
            writer: seal::random_id(),
        }
    }

    pub fn cache_stats(&self) -> (u64, u64, usize) {
        (self.cache.hits, self.cache.misses, self.cache.bytes)
    }

    fn k_registry(&self) -> String {
        format!("{}registry", self.prefix)
    }
    pub fn k_manifest(&self, repo: &str) -> String {
        format!("{}r/{repo}/manifest", self.prefix)
    }
    fn k_pack(&self, repo: &str, pack: &str) -> String {
        format!("{}r/{repo}/{pack}.pack", self.prefix)
    }
    fn k_idx(&self, repo: &str, pack: &str) -> String {
        format!("{}r/{repo}/{pack}.idx", self.prefix)
    }
    pub fn k_object(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    fn get_sealed(
        &mut self,
        key: &str,
        scope: &str,
        etag: Option<&str>,
    ) -> Result<Loaded<Vec<u8>>, String> {
        match self.s3.get(key, etag)? {
            None => Ok(Loaded::Missing),
            Some(g) if g.status == 304 => Ok(Loaded::NotModified),
            Some(g) => {
                let tag = g
                    .etag
                    .ok_or("storage returned no ETag; conditional writes are required")?;
                let plain = seal::open(&self.keys.derive(scope), key.as_bytes(), &g.body)?;
                Ok(Loaded::Found(decompress(&plain)?, tag))
            }
        }
    }

    fn put_sealed(
        &mut self,
        key: &str,
        scope: &str,
        plain: &[u8],
        etag: Option<&str>,
    ) -> Result<Saved, String> {
        let body = seal::seal(&self.keys.derive(scope), key.as_bytes(), &compress(plain));
        let cond = match etag {
            Some(t) => Cond::IfMatch(t),
            None => Cond::IfNoneMatch,
        };
        match self.s3.put(key, &body, cond)? {
            Ok(Some(t)) => Ok(Saved::Ok(t)),
            Ok(None) => Err("storage omitted the ETag of a committed write".into()),
            Err(_) => Ok(Saved::Conflict),
        }
    }

    pub fn load_registry(&mut self, etag: Option<&str>) -> Result<Loaded<Registry>, String> {
        let key = self.k_registry();
        Ok(match self.get_sealed(&key, "registry", etag)? {
            Loaded::Found(b, t) => {
                let r: Registry = serde_json::from_slice(&b)
                    .map_err(|e| format!("registry does not parse: {e}"))?;
                if r.v != 1 {
                    return Err(format!(
                        "registry format v{} is newer than this server",
                        r.v
                    ));
                }
                Loaded::Found(r, t)
            }
            Loaded::Missing => Loaded::Missing,
            Loaded::NotModified => Loaded::NotModified,
        })
    }

    pub fn save_registry(&mut self, r: &mut Registry, etag: Option<&str>) -> Result<Saved, String> {
        r.v = 1;
        r.rev += 1;
        r.writer = self.writer.clone();
        let key = self.k_registry();
        let b = serde_json::to_vec(r).unwrap();
        self.put_sealed(&key, "registry", &b, etag)
    }

    pub fn load_manifest(
        &mut self,
        repo: &str,
        etag: Option<&str>,
    ) -> Result<Loaded<Manifest>, String> {
        let key = self.k_manifest(repo);
        Ok(
            match self.get_sealed(&key, &format!("repo:{repo}"), etag)? {
                Loaded::Found(b, t) => {
                    let m: Manifest = serde_json::from_slice(&b)
                        .map_err(|e| format!("manifest does not parse: {e}"))?;
                    if m.v != 1 {
                        return Err(format!(
                            "manifest format v{} is newer than this server",
                            m.v
                        ));
                    }
                    Loaded::Found(m, t)
                }
                Loaded::Missing => Loaded::Missing,
                Loaded::NotModified => Loaded::NotModified,
            },
        )
    }

    pub fn save_manifest(
        &mut self,
        repo: &str,
        m: &mut Manifest,
        etag: Option<&str>,
        now: u64,
    ) -> Result<Saved, String> {
        m.v = 1;
        m.rev += 1;
        m.writer = format!("{}:{}", self.writer, seal::random_id());
        m.updated = now;
        let key = self.k_manifest(repo);
        let b = serde_json::to_vec(m).unwrap();
        self.put_sealed(&key, &format!("repo:{repo}"), &b, etag)
    }

    pub fn put_idx(&mut self, repo: &str, pack: &str, entries: &[IdxEntry]) -> Result<(), String> {
        let key = self.k_idx(repo, pack);
        let body = seal::seal(
            &self.keys.derive(&format!("repo:{repo}")),
            key.as_bytes(),
            &compress(&repo::encode(entries)),
        );
        match self.s3.put(&key, &body, Cond::None)? {
            Ok(_) => Ok(()),
            Err(s) => Err(format!("storage refused the pack index (HTTP {s})")),
        }
    }

    pub fn get_idx(&mut self, repo: &str, pack: &str) -> Result<Vec<IdxEntry>, String> {
        let key = self.k_idx(repo, pack);
        match self.get_sealed(&key, &format!("repo:{repo}"), None)? {
            Loaded::Found(b, _) => repo::decode(&b),
            _ => Err(format!("pack index {pack} is missing from storage")),
        }
    }

    fn chunked(&self, repo: &str, pack: &str, len: u64, chunk: u32) -> Chunked {
        let key = self.k_pack(repo, pack);
        Chunked::new(
            &self.keys.derive(&format!("pack:{repo}:{pack}")),
            &key,
            len,
            chunk,
        )
    }

    pub fn upload_begin(&mut self, repo: &str, pack: &str, len: u64) -> Result<Upload, String> {
        let chunked = self.chunked(repo, pack, len, CHUNK);
        let key = self.k_pack(repo, pack);
        let upload_id = if chunked.chunks() > PART_CHUNKS {
            Some(self.s3.create_multipart(&key)?)
        } else {
            None
        };
        Ok(Upload {
            key,
            chunked,
            upload_id,
            next: 0,
            parts: Vec::new(),
            sent: 0,
        })
    }

    /// The plaintext range the next call to `upload_part` must carry.
    pub fn next_part(&self, up: &Upload) -> (u64, u64) {
        let c = &up.chunked;
        let n = c.chunks();
        let last = if up.upload_id.is_none() {
            n
        } else {
            (up.next + PART_CHUNKS).min(n)
        };
        (
            up.next * c.chunk as u64,
            (last * c.chunk as u64).min(c.total),
        )
    }

    /// Seal and send the next part, `plain` being exactly the range
    /// `next_part` names. Ok(true) once the object is complete.
    pub fn upload_part(&mut self, up: &mut Upload, plain: &[u8]) -> Result<bool, String> {
        let (a, b) = self.next_part(up);
        if plain.len() as u64 != b - a {
            return Err("internal: upload part has the wrong length".into());
        }
        let c = &up.chunked;
        let n = c.chunks();
        let first = up.next;
        let last = first + (b - a).div_ceil(c.chunk as u64);
        let mut body =
            Vec::with_capacity(((last - first) * (c.chunk as u64 + TAG as u64)) as usize);
        for i in first..last {
            let s = ((i - first) * c.chunk as u64) as usize;
            let mut x = plain[s..s + c.plain_len(i) as usize].to_vec();
            c.seal_chunk(i, &mut x);
            body.extend_from_slice(&x);
        }
        match up.upload_id.clone() {
            None => {
                match self.s3.put(&up.key, &body, Cond::None)? {
                    Ok(_) => {}
                    Err(s) => return Err(format!("storage refused the pack (HTTP {s})")),
                }
                up.next = n;
                up.sent = c.total;
                Ok(true)
            }
            Some(id) => {
                let etag = self
                    .s3
                    .upload_part(&up.key, &id, up.parts.len() as u32 + 1, &body)?;
                up.parts.push(etag);
                up.next = last;
                up.sent = b;
                if last < n {
                    return Ok(false);
                }
                self.s3.complete_multipart(&up.key, &id, &up.parts)?;
                // the store must now hold exactly the sealed length
                let want = up.chunked.cipher_total();
                match self.s3.head_size(&up.key)? {
                    Some(sz) if sz == want => Ok(true),
                    other => Err(format!("stored pack has size {other:?}, expected {want}")),
                }
            }
        }
    }

    /// Plaintext of a stored pack without going through (or filling) the
    /// chunk cache: bulk copies must not evict what fetches are using.
    pub fn read_pack_uncached(
        &mut self,
        repo: &str,
        p: &PackInfo,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, String> {
        let c = self.chunked(repo, &p.id, p.len, p.chunk);
        let cs = c.chunk as u64;
        let (first, last) = (start / cs, (end - 1) / cs);
        let (a, b) = (
            c.cipher_offset(first),
            c.cipher_offset(last) + c.plain_len(last) + TAG as u64,
        );
        let raw = self.s3.get_range(&self.k_pack(repo, &p.id), a, b)?;
        let mut out = Vec::with_capacity((end - start) as usize);
        let mut off = 0usize;
        for k in first..=last {
            let n = (c.plain_len(k) + TAG as u64) as usize;
            let mut x = raw[off..off + n].to_vec();
            off += n;
            c.open_chunk(k, &mut x)?;
            let base = k * cs;
            let s = start.max(base) - base;
            let e = end.min(base + x.len() as u64) - base;
            out.extend_from_slice(&x[s as usize..e as usize]);
        }
        Ok(out)
    }

    pub fn upload_abort(&mut self, up: &Upload) {
        if let Some(id) = &up.upload_id {
            self.s3.abort_multipart(&up.key, id);
        }
        let _ = self.s3.delete(&up.key);
    }

    /// Plaintext bytes [start, end) of a stored pack, through the chunk cache.
    pub fn read_pack(
        &mut self,
        repo: &str,
        p: &PackInfo,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, String> {
        if end > p.len || start > end {
            return Err("read beyond the end of a stored pack".into());
        }
        if start == end {
            return Ok(Vec::new());
        }
        let c = self.chunked(repo, &p.id, p.len, p.chunk);
        let tag = pack_tag(&p.id) ^ pack_tag(repo).rotate_left(17);
        let cs = c.chunk as u64;
        let (first, lastc) = (start / cs, (end - 1) / cs);
        let mut chunks: Vec<Rc<Vec<u8>>> = Vec::with_capacity((lastc - first + 1) as usize);
        let mut i = first;
        while i <= lastc {
            if let Some(v) = self.cache.get((tag, i)) {
                chunks.push(v);
                i += 1;
                continue;
            }
            // a run of missing chunks, read with one ranged GET
            let mut j = i;
            while j < lastc
                && j - i + 1 < FETCH_CHUNKS
                && !self.cache.map.contains_key(&(tag, j + 1))
            {
                j += 1;
            }
            let (a, b) = (
                c.cipher_offset(i),
                c.cipher_offset(j) + c.plain_len(j) + TAG as u64,
            );
            let raw = self.s3.get_range(&c_key(self, repo, &p.id), a, b)?;
            self.cache.misses += j - i + 1;
            let mut off = 0usize;
            for k in i..=j {
                let n = (c.plain_len(k) + TAG as u64) as usize;
                let mut b = raw[off..off + n].to_vec();
                off += n;
                c.open_chunk(k, &mut b)?;
                let v = Rc::new(b);
                self.cache.put((tag, k), v.clone());
                chunks.push(v);
            }
            i = j + 1;
        }
        let mut out = Vec::with_capacity((end - start) as usize);
        for (n, ch) in chunks.iter().enumerate() {
            let base = (first + n as u64) * cs;
            let s = start.max(base) - base;
            let e = end.min(base + ch.len() as u64) - base;
            out.extend_from_slice(&ch[s as usize..e as usize]);
        }
        Ok(out)
    }

    pub fn delete_object(&mut self, name: &str) -> Result<(), String> {
        let key = self.k_object(name);
        self.s3.delete(&key)
    }

    pub fn delete_pack(&mut self, repo: &str, pack: &str) -> Result<(), String> {
        let (a, b) = (self.k_pack(repo, pack), self.k_idx(repo, pack));
        self.s3.delete(&a)?;
        self.s3.delete(&b)
    }

    /// Every object under a repository's prefix (for deletion).
    pub fn delete_repo_objects(&mut self, repo: &str) -> Result<usize, String> {
        let prefix = format!("{}r/{repo}/", self.prefix);
        let mut n = 0;
        loop {
            let (keys, _) = self.s3.list(&prefix, None)?;
            if keys.is_empty() {
                return Ok(n);
            }
            for (k, _, _) in keys {
                self.s3.delete(&k)?;
                n += 1;
            }
        }
    }

    /// Every object of a repository: (name under `r/<id>/`, last modified).
    pub fn list_repo(&mut self, repo: &str) -> Result<Vec<(String, u64)>, String> {
        let prefix = format!("{}r/{repo}/", self.prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let (keys, next) = self.s3.list(&prefix, token.as_deref())?;
            out.extend(
                keys.into_iter()
                    .filter_map(|(k, _, m)| k.strip_prefix(&prefix).map(|n| (n.to_string(), m))),
            );
            match next {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    pub fn get_raw_sealed(
        &mut self,
        name: &str,
        scope: &str,
    ) -> Result<Option<(Vec<u8>, String)>, String> {
        let key = self.k_object(name);
        Ok(match self.get_sealed(&key, scope, None)? {
            Loaded::Found(b, t) => Some((b, t)),
            _ => None,
        })
    }

    pub fn put_raw_sealed(
        &mut self,
        name: &str,
        scope: &str,
        plain: &[u8],
        etag: Option<&str>,
    ) -> Result<Saved, String> {
        let key = self.k_object(name);
        self.put_sealed(&key, scope, plain, etag)
    }
}

fn c_key(s: &Store, repo: &str, pack: &str) -> String {
    s.k_pack(repo, pack)
}
