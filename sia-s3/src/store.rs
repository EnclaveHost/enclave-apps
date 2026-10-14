//! The S3 key space over Sia objects, with no local state.
//!
//! Every Sia object this app pins carries JSON metadata (sealed by the SDK, so
//! the indexer sees ciphertext). That metadata lists the S3 keys the object
//! serves, which makes the indexer's object feed the whole index:
//!
//!   - PUT uploads the body as one object whose metadata names the key.
//!   - CopyObject adds an entry to the source's host object: no data moves.
//!   - DELETE removes the entry; an object left with no entries is deleted.
//!   - Multipart parts are objects tagged `mp`; Complete writes the key's entry
//!     on the first part and lists the others (`x`) in order.
//!   - The compactor repacks small single-object keys into shared slabs (a
//!     lone small upload occupies a full slab: 4 MiB sectors on 30 hosts).
//!
//! Remote writes happen first; the in-memory index changes only after they
//! succeed, so a failed request leaves both views as they were.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use sia_storage::{DownloadOptions, Object, ObjectsCursor, PackedUploadOptions, Sdk, UploadOptions};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

pub type Res<T> = Result<T, String>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub b: String,
    pub k: String,
    /// size in bytes
    pub s: u64,
    /// ETag (no quotes)
    pub t: String,
    /// last modified, unix seconds
    pub m: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
    /// further part objects after the host, in order (multipart keys)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub x: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PartTag {
    pub u: String,
    pub b: String,
    pub k: String,
    pub n: u32,
    pub t: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub v: u32,
    /// already packed with others (compactor output, or large)
    #[serde(default)]
    pub p: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub e: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mp: Option<PartTag>,
}

pub struct Key {
    pub host: String,
    pub entry: Entry,
}

pub struct Upload {
    pub b: String,
    pub k: String,
    pub started: i64,
    /// part number -> (object id, etag, size)
    pub parts: BTreeMap<u32, (String, String, u64)>,
}

#[derive(Default)]
pub struct Index {
    pub keys: BTreeMap<(String, String), Key>,
    pub objs: HashMap<String, (Object, Meta)>,
    pub uploads: HashMap<String, Upload>,
    /// objects with our metadata that serve nothing (found at boot)
    pub orphans: Vec<String>,
}

#[derive(Default, Clone, Serialize)]
pub struct Stats {
    pub compactions: u64,
    pub compacted_objects: u64,
    pub last_compaction: Option<String>,
    pub last_error: Option<String>,
}

pub struct Store {
    pub sdk: Sdk,
    pub idx: Mutex<Index>,
    pub stats: std::cell::RefCell<Stats>,
    pub compact_max_object: u64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn e2s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn upload_opts() -> UploadOptions {
    UploadOptions { max_buffered_slabs: Some(1), ..Default::default() }
}

pub fn md5_hex(b: &[u8]) -> String {
    hex::encode(Md5::digest(b))
}

fn new_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("wasi random");
    hex::encode(b)
}

impl Index {
    pub fn parts(&self, k: &Key) -> Vec<String> {
        std::iter::once(k.host.clone()).chain(k.entry.x.iter().cloned()).collect()
    }

    /// The (object, offset, length) reads that cover [start, start+len) of a key.
    pub fn segments(&self, k: &Key, start: u64, len: u64) -> Res<Vec<(Object, u64, u64)>> {
        let mut out = Vec::new();
        let (mut pos, end) = (0u64, start + len);
        for id in self.parts(k) {
            let (obj, _) = self.objs.get(&id).ok_or_else(|| format!("part {id} is missing from the index"))?;
            // single-object keys may hold a 1-byte placeholder for an empty body
            let size = if k.entry.x.is_empty() { k.entry.s } else { obj.size() };
            let (a, b) = (pos, pos + size);
            if b > start && a < end {
                let from = start.max(a) - a;
                let to = end.min(b) - a;
                out.push((obj.clone(), from, to - from));
            }
            pos = b;
            if pos >= end {
                break;
            }
        }
        Ok(out)
    }

    fn referenced_elsewhere(&self, id: &str, except: &(String, String)) -> bool {
        self.keys.iter().any(|(kk, k)| kk != except && (k.host == id || k.entry.x.iter().any(|x| x == id)))
    }
}

impl Store {
    /// Rebuild the index from the indexer's object feed.
    pub async fn load(sdk: Sdk, compact_max_object: u64) -> Res<Store> {
        let mut latest: HashMap<String, Option<Object>> = HashMap::new();
        let mut cursor: Option<ObjectsCursor> = None;
        let mut events = 0usize;
        loop {
            let evs = sdk.object_events(cursor.take(), Some(500)).await.map_err(e2s)?;
            if evs.is_empty() {
                break;
            }
            events += evs.len();
            for e in &evs {
                latest.insert(e.id.to_string(), if e.deleted { None } else { e.object.clone() });
            }
            let last = evs.last().unwrap();
            cursor = Some(ObjectsCursor { id: last.id, after: last.updated_at });
            if evs.len() < 500 {
                break;
            }
        }

        let mut idx = Index::default();
        let mut foreign = 0;
        for (id, obj) in latest {
            let Some(obj) = obj else { continue };
            match serde_json::from_slice::<Meta>(&obj.metadata) {
                Ok(m) if m.v == 1 => {
                    idx.objs.insert(id, (obj, m));
                }
                _ => foreign += 1,
            }
        }
        let mut dups = 0;
        for (id, (_, m)) in &idx.objs {
            for e in &m.e {
                let kk = (e.b.clone(), e.k.clone());
                let newer = idx.keys.get(&kk).is_none_or(|cur| e.m > cur.entry.m);
                if idx.keys.contains_key(&kk) {
                    dups += 1;
                }
                if newer {
                    idx.keys.insert(kk, Key { host: id.clone(), entry: e.clone() });
                }
            }
        }
        let referenced: HashSet<String> = idx.keys.values().flat_map(|k| idx.parts(k)).collect();
        for (id, (obj, m)) in &idx.objs {
            if referenced.contains(id) {
                continue;
            }
            if let Some(t) = &m.mp {
                let u = idx.uploads.entry(t.u.clone()).or_insert_with(|| Upload {
                    b: t.b.clone(),
                    k: t.k.clone(),
                    started: obj.created_at.timestamp(),
                    parts: BTreeMap::new(),
                });
                u.parts.insert(t.n, (id.clone(), t.t.clone(), obj.size()));
                u.started = u.started.min(obj.created_at.timestamp());
            } else {
                idx.orphans.push(id.clone());
            }
        }
        eprintln!(
            "[sia-s3] index: {events} events, {} objects, {} keys, {} open uploads, {} orphans, {dups} superseded entries, {foreign} foreign objects",
            idx.objs.len(),
            idx.keys.len(),
            idx.uploads.len(),
            idx.orphans.len()
        );
        Ok(Store { sdk, idx: Mutex::new(idx), stats: Default::default(), compact_max_object })
    }

    async fn pin_with(&self, obj: &mut Object, meta: &Meta) -> Res<()> {
        obj.metadata = serde_json::to_vec(meta).map_err(e2s)?;
        self.sdk.pin_object(obj).await.map_err(e2s)
    }

    async fn upload(&self, body: Vec<u8>) -> Res<Object> {
        // the indexer refuses slab-less objects: an empty body is stored as one
        // placeholder byte and served as empty (the entry's size is 0)
        let data = if body.is_empty() { vec![0u8] } else { body };
        self.sdk.upload(Object::default(), Cursor::new(data), upload_opts()).await.map_err(e2s)
    }

    /// Remove a key's entry from its host object (remote first), deleting objects
    /// left serving nothing. The caller removes the key from `idx.keys` after.
    async fn detach(&self, idx: &mut Index, kk: &(String, String)) -> Res<()> {
        let Some(key) = idx.keys.get(kk) else { return Ok(()) };
        let (host, entry) = (key.host.clone(), key.entry.clone());
        let Some((obj, meta)) = idx.objs.get(&host).cloned() else { return Ok(()) };
        let mut nm = meta.clone();
        nm.e.retain(|e| !(e.b == entry.b && e.k == entry.k));
        if nm.e.is_empty() && nm.mp.is_none() {
            let mut gone = vec![host.clone()];
            for x in &entry.x {
                if !idx.referenced_elsewhere(x, kk) {
                    gone.push(x.clone());
                }
            }
            for id in &gone {
                if let Some((o, _)) = idx.objs.get(id) {
                    self.sdk.delete_object(&o.id()).await.map_err(e2s)?;
                }
                idx.objs.remove(id);
            }
        } else {
            let mut o = obj.clone();
            self.pin_with(&mut o, &nm).await?;
            idx.objs.insert(host, (o, nm));
        }
        Ok(())
    }

    pub async fn put(&self, b: &str, k: &str, body: Vec<u8>, ct: Option<String>) -> Res<String> {
        let etag = md5_hex(&body);
        let size = body.len() as u64;
        let mut obj = self.upload(body).await?;
        let entry = Entry { b: b.into(), k: k.into(), s: size, t: etag.clone(), m: now(), c: ct, x: vec![] };
        let meta = Meta { v: 1, p: false, e: vec![entry.clone()], mp: None };
        self.pin_with(&mut obj, &meta).await?;
        let id = obj.id().to_string();
        let mut idx = self.idx.lock().await;
        let kk = (b.to_string(), k.to_string());
        // the new object is pinned and authoritative (newest mtime) even if the
        // old entry cannot be removed now; a stale entry loses at the next boot
        if let Err(e) = self.detach(&mut idx, &kk).await {
            eprintln!("[sia-s3] put {b}/{k}: old entry left behind: {e}");
        }
        idx.objs.insert(id.clone(), (obj, meta));
        idx.keys.insert(kk, Key { host: id, entry });
        Ok(etag)
    }

    pub async fn copy(&self, src: (&str, &str), dst: (&str, &str)) -> Res<(String, i64)> {
        let mut idx = self.idx.lock().await;
        let skk = (src.0.to_string(), src.1.to_string());
        let dkk = (dst.0.to_string(), dst.1.to_string());
        let (host, sentry) = match idx.keys.get(&skk) {
            Some(k) => (k.host.clone(), k.entry.clone()),
            None => return Err("NoSuchKey".into()),
        };
        let mut entry = sentry.clone();
        entry.b = dkk.0.clone();
        entry.k = dkk.1.clone();
        entry.m = now();
        if skk == dkk {
            return Ok((entry.t, sentry.m));
        }
        // the destination's current entry goes first: it may live on this same host
        if idx.keys.contains_key(&dkk) {
            self.detach(&mut idx, &dkk).await?;
            idx.keys.remove(&dkk);
        }
        let (obj, meta) = idx.objs.get(&host).cloned().ok_or("source object missing from the index")?;
        let mut nm = meta.clone();
        nm.e.retain(|e| !(e.b == entry.b && e.k == entry.k));
        nm.e.push(entry.clone());
        let mut o = obj.clone();
        self.pin_with(&mut o, &nm).await?;
        idx.objs.insert(host.clone(), (o, nm));
        let m = entry.m;
        let t = entry.t.clone();
        idx.keys.insert(dkk, Key { host, entry });
        Ok((t, m))
    }

    pub async fn delete(&self, b: &str, k: &str) -> Res<()> {
        let mut idx = self.idx.lock().await;
        let kk = (b.to_string(), k.to_string());
        if !idx.keys.contains_key(&kk) {
            return Ok(()); // S3: deleting a missing key succeeds
        }
        self.detach(&mut idx, &kk).await?;
        idx.keys.remove(&kk);
        Ok(())
    }

    pub async fn create_upload(&self, b: &str, k: &str) -> String {
        let id = new_id();
        self.idx.lock().await.uploads.insert(
            id.clone(),
            Upload { b: b.into(), k: k.into(), started: now(), parts: BTreeMap::new() },
        );
        id
    }

    pub async fn upload_part(&self, uid: &str, n: u32, body: Vec<u8>) -> Res<String> {
        let (b, k) = {
            let idx = self.idx.lock().await;
            let u = idx.uploads.get(uid).ok_or("NoSuchUpload")?;
            (u.b.clone(), u.k.clone())
        };
        let etag = md5_hex(&body);
        let size = body.len() as u64;
        let mut obj = self.upload(body).await?;
        let meta = Meta { v: 1, p: size >= self.compact_max_object, e: vec![], mp: Some(PartTag { u: uid.into(), b, k, n, t: etag.clone() }) };
        self.pin_with(&mut obj, &meta).await?;
        let id = obj.id().to_string();
        let mut idx = self.idx.lock().await;
        let old = match idx.uploads.get_mut(uid) {
            Some(u) => u.parts.insert(n, (id.clone(), etag.clone(), size)),
            None => {
                drop(idx);
                let _ = self.sdk.delete_object(&obj.id()).await; // aborted meanwhile
                return Err("NoSuchUpload".into());
            }
        };
        idx.objs.insert(id, (obj, meta));
        if let Some((old_id, _, _)) = old {
            if let Some((o, _)) = idx.objs.remove(&old_id) {
                let _ = self.sdk.delete_object(&o.id()).await;
            }
        }
        Ok(etag)
    }

    pub async fn complete_upload(&self, uid: &str, want: &[(u32, String)]) -> Res<(String, String, String)> {
        let mut idx = self.idx.lock().await;
        let u = idx.uploads.get(uid).ok_or("NoSuchUpload")?;
        if want.is_empty() {
            return Err("InvalidPart: no parts listed".into());
        }
        let mut ids = Vec::new();
        let mut md5s = Vec::new();
        let mut size = 0u64;
        let mut last = 0u32;
        for (n, etag) in want {
            if *n <= last {
                return Err("InvalidPartOrder".into());
            }
            last = *n;
            let (id, t, s) = u.parts.get(n).ok_or_else(|| format!("InvalidPart: part {n} was not uploaded"))?;
            if t != etag.trim_matches('"') {
                return Err(format!("InvalidPart: part {n} etag does not match"));
            }
            ids.push(id.clone());
            md5s.extend(hex::decode(t).map_err(e2s)?);
            size += s;
        }
        let etag = format!("{}-{}", md5_hex(&md5s), want.len());
        let (b, k) = (u.b.clone(), u.k.clone());
        let unused: Vec<String> = u.parts.values().map(|p| p.0.clone()).filter(|id| !ids.contains(id)).collect();
        let entry = Entry { b: b.clone(), k: k.clone(), s: size, t: etag.clone(), m: now(), c: None, x: ids[1..].to_vec() };

        let kk = (b.clone(), k.clone());
        if idx.keys.contains_key(&kk) {
            self.detach(&mut idx, &kk).await?;
            idx.keys.remove(&kk);
        }
        let host = ids[0].clone();
        let (obj, meta) = idx.objs.get(&host).cloned().ok_or("part object missing from the index")?;
        let nm = Meta { v: 1, p: meta.p, e: vec![entry.clone()], mp: None };
        let mut o = obj.clone();
        self.pin_with(&mut o, &nm).await?;
        idx.objs.insert(host.clone(), (o, nm));
        idx.keys.insert(kk, Key { host, entry });
        idx.uploads.remove(uid);
        for id in unused {
            if let Some((o, _)) = idx.objs.remove(&id) {
                let _ = self.sdk.delete_object(&o.id()).await;
            }
        }
        Ok((b, k, etag))
    }

    pub async fn abort_upload(&self, uid: &str) -> Res<()> {
        let mut idx = self.idx.lock().await;
        let u = idx.uploads.remove(uid).ok_or("NoSuchUpload")?;
        for (id, _, _) in u.parts.values() {
            if let Some((o, _)) = idx.objs.remove(id) {
                self.sdk.delete_object(&o.id()).await.map_err(e2s)?;
            }
        }
        Ok(())
    }

    /// Stream `[off, off+len)` of one object into `w`.
    pub async fn read_into<W: tokio::io::AsyncWrite + Unpin>(&self, obj: &Object, off: u64, len: u64, w: &mut W) -> Res<u64> {
        if len == 0 {
            return Ok(0);
        }
        let opts = DownloadOptions { max_buffered_chunks: Some(8), offset: off, length: Some(len), ..Default::default() };
        let mut rd = self.sdk.download(obj, opts).map_err(e2s)?;
        tokio::io::copy(&mut rd, w).await.map_err(e2s)
    }

    /// Repack small keys that were uploaded alone into shared slabs.
    pub async fn compact(&self) -> Res<usize> {
        let cutoff = now() - 300;
        let cands: Vec<(String, Object, Meta)> = {
            let idx = self.idx.lock().await;
            let mut total = 0u64;
            let mut v = Vec::new();
            for (id, (obj, m)) in &idx.objs {
                let small = obj.size() <= self.compact_max_object;
                let simple = !m.p && m.mp.is_none() && !m.e.is_empty() && m.e.iter().all(|e| e.x.is_empty());
                if small && simple && obj.created_at.timestamp() < cutoff && total + obj.size() <= 64 << 20 {
                    total += obj.size();
                    v.push((id.clone(), obj.clone(), m.clone()));
                }
            }
            v
        };
        if cands.len() < 2 {
            return Ok(0);
        }
        let mut datas = Vec::with_capacity(cands.len());
        for (_, obj, _) in &cands {
            let mut rd = self.sdk.download(obj, DownloadOptions { max_buffered_chunks: Some(8), ..Default::default() }).map_err(e2s)?;
            let mut buf = Vec::with_capacity(obj.size() as usize);
            rd.read_to_end(&mut buf).await.map_err(e2s)?;
            datas.push(buf);
        }
        let mut pu = self
            .sdk
            .upload_packed(PackedUploadOptions { max_buffered_slabs: Some(1), ..Default::default() })
            .map_err(e2s)?;
        for d in datas {
            pu.add(Cursor::new(d)).await.map_err(e2s)?;
        }
        let news = pu.finalize().await.map_err(e2s)?;
        if news.len() != cands.len() {
            return Err(format!("packed upload returned {} objects for {}", news.len(), cands.len()));
        }

        let mut done = 0;
        let mut idx = self.idx.lock().await;
        for ((old_id, _, old_meta), mut new_obj) in cands.into_iter().zip(news) {
            // the key set may have changed while we were uploading; then this copy is
            // simply never pinned and prune_slabs reclaims it
            let unchanged = idx.objs.get(&old_id).is_some_and(|(_, m)| *m == old_meta);
            if !unchanged {
                continue;
            }
            let mut nm = old_meta.clone();
            nm.p = true;
            if let Err(e) = self.pin_with(&mut new_obj, &nm).await {
                eprintln!("[sia-s3] compact: pin failed: {e}");
                continue;
            }
            let new_id = new_obj.id().to_string();
            if let Some((o, _)) = idx.objs.get(&old_id) {
                if let Err(e) = self.sdk.delete_object(&o.id()).await {
                    // both copies now name the keys; the boot rebuild tolerates that
                    eprintln!("[sia-s3] compact: old object not deleted: {e}");
                }
            }
            idx.objs.remove(&old_id);
            for e in &nm.e {
                if let Some(k) = idx.keys.get_mut(&(e.b.clone(), e.k.clone())) {
                    if k.host == old_id {
                        k.host = new_id.clone();
                    }
                }
            }
            idx.objs.insert(new_id, (new_obj, nm));
            done += 1;
        }
        drop(idx);
        if let Err(e) = self.sdk.prune_slabs(None).await {
            eprintln!("[sia-s3] compact: prune_slabs: {e}");
        }
        Ok(done)
    }

    /// Boot-time cleanup: orphans (serve no key) and uploads abandoned for a week.
    pub async fn sweep(&self) {
        let mut idx = self.idx.lock().await;
        let orphans = std::mem::take(&mut idx.orphans);
        for id in orphans {
            if let Some((o, _)) = idx.objs.get(&id) {
                if o.created_at.timestamp() < now() - 86400 {
                    if self.sdk.delete_object(&o.id()).await.is_ok() {
                        idx.objs.remove(&id);
                    }
                }
            }
        }
        let stale: Vec<String> = idx.uploads.iter().filter(|(_, u)| u.started < now() - 7 * 86400).map(|(k, _)| k.clone()).collect();
        for uid in stale {
            if let Some(u) = idx.uploads.remove(&uid) {
                for (id, _, _) in u.parts.values() {
                    if let Some((o, _)) = idx.objs.remove(id) {
                        let _ = self.sdk.delete_object(&o.id()).await;
                    }
                }
            }
        }
    }
}
