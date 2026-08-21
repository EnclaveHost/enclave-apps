//! s3-ipfs-adapter: expose an S3 bucket's objects as IPFS content, from
//! inside an attested enclave.
//!
//! The app connects to a configured S3-compatible bucket (the risc-box S3
//! path: SigV4 over the platform's transparent egress), walks every object,
//! and computes the exact UnixFS merkle DAG that `ipfs add --cid-version 1`
//! would mint: 256 KiB chunks, raw leaves, balanced layout, CIDv1/sha2-256.
//! It then serves the DAG over the standard IPFS gateway surface:
//!
//!   GET /ipfs/<cid>[/<path>]      path gateway (files, dirs, index.html,
//!                                 HTTP ranges, HEAD)
//!   GET /ipfs/<cid>?format=raw    one verified block (trustless gateway)
//!   GET /ipfs/<cid>?format=car    the DAG as a CARv1 stream
//!   GET /                         UI: bucket listing with per-file CIDs
//!   GET /api/status /api/files    JSON; POST /api/refresh re-lists
//!
//! What is held in memory is only the merkle SKELETON: dag-pb nodes and one
//! 32-byte digest per 256 KiB chunk. File bytes stay in S3 and are fetched
//! by byte range on demand, hash-verified against the index before a single
//! byte is served, so the gateway can never silently serve bytes that do not
//! match the CID (an object changed under us surfaces as a truncated
//! response and a log line, and the next refresh re-indexes it).
//!
//! Indexing shares the single-threaded event loop: one bounded S3 request
//! per tick (a LIST page or a 4 MiB hash window), so the UI and the gateway
//! stay live while a large bucket indexes, and a refresh re-hashes only
//! objects whose (size, etag) changed. Serving always runs from an immutable
//! snapshot (Rc), so long downloads survive a concurrent re-index.
//!
//! Trust: CIDs are self-verifying, so a client that checks hashes (any IPFS
//! client, `ipfs dag import` on a fetched CAR) needs to trust neither this
//! app nor S3. The enclave adds the other half: the attested build is what
//! computed the index, so the published root CID is a faithful commitment
//! to the bucket's contents at index time.

mod egress;
mod httpd;
mod imgcheck;
mod ipfs;
mod leavescache;
mod redirects;
mod s3;
mod upload;
mod wasmscan;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use httpd::{json, json_escape, Body, Request, Response, Server};
use ipfs::{Cid, CHUNK, CODEC_DAG_PB, CODEC_RAW};

const UI: &str = include_str!("ui.html");
const APP: &str = concat!("s3-ipfs-adapter/", env!("CARGO_PKG_VERSION"));

const MAX_BODY: usize = 16 * 1024;
/// Upload cap. Bodies buffer in memory (the engine has no request
/// streaming), so this is a deliberate ceiling; bigger objects belong in
/// S3 tooling, this is an admin convenience.
const MAX_UPLOAD: usize = 32 * 1024 * 1024;
/// Bytes fetched per indexing tick (whole chunks; 16 chunks).
const INDEX_WINDOW: u64 = 4 * 1024 * 1024;
/// Derived-state directory (under the configured prefix): the persisted
/// leaves cache lives here, and everything under it is EXCLUDED from the
/// index — it describes the bucket, it is not content.
const CACHE_DIR_REL: &str = ".enclave-index/";
const CACHE_REL: &str = ".enclave-index/leaves.bin";
/// Read ceiling for the cache object (~600 KB at today's 1000-file bucket;
/// a cache past this parses as truncated and degrades to a full re-hash).
const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Bytes fetched per streaming pull (whole chunks; 8 chunks).
const STREAM_WINDOW: u64 = 2 * 1024 * 1024;
/// Soft target for one CAR pull's output.
const CAR_TARGET: usize = 256 * 1024;
/// Buffered responses above this go out as a stream instead (the engine
/// closes buffered clients that sit on more than MAX_WBUF).
const DIRECT_MAX: usize = 256 * 1024;

// ---- config ----------------------------------------------------------------

struct Config {
    title: String,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    creds: Option<s3::Creds>,
    refresh_secs: u64,
    max_keys: usize,
    api_key: Option<String>,
    upload: Option<upload::UploadCfg>,
}

fn creds_from(v: &serde_json::Value) -> Option<s3::Creds> {
    let ak = v.get("accessKeyId").and_then(|x| x.as_str())?;
    let sk = v.get("secretAccessKey").and_then(|x| x.as_str())?;
    if ak.is_empty() || sk.is_empty() {
        return None;
    }
    Some(s3::Creds {
        access_key_id: ak.to_string(),
        secret_access_key: sk.to_string(),
        session_token: v
            .get("sessionToken")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Resolves config string values of the exact form `$NAME` / `${NAME}` from
/// the process environment. Deployment secrets arrive as env vars, so this is
/// what lets a config reference them instead of inlining values. Whole-value
/// references only, no interpolation inside larger strings. An unresolved
/// reference becomes "" (with a log line naming it), which downstream treats
/// as absent.
fn expand_env_refs(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            let Some(reference) = s.strip_prefix('$') else { return };
            let name = reference.strip_prefix('{').and_then(|r| r.strip_suffix('}')).unwrap_or(reference);
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || name.starts_with(|c: char| c.is_ascii_digit())
            {
                return; // not a $NAME reference; leave the literal alone
            }
            match std::env::var(name) {
                Ok(val) => {
                    eprintln!("[s3-ipfs-adapter] config: resolved ${name} from the environment");
                    *s = val;
                }
                Err(_) => {
                    eprintln!("[s3-ipfs-adapter] config: ${name} is not set in the environment; treating the value as absent");
                    s.clear();
                }
            }
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(expand_env_refs),
        serde_json::Value::Array(items) => items.iter_mut().for_each(expand_env_refs),
        _ => {}
    }
}

/// Reads the config, always returning one. Missing or unresolved fields are
/// left empty rather than fatal: a fresh deployment whose `$VAR` secrets are
/// not set yet must still START and serve the UI so they can be provided
/// (and the process restarted).
fn load_config() -> Config {
    let raw = std::env::var("ENCLAVE_CONFIG")
        .or_else(|_| std::env::var("S3IPFS_CONFIG"))
        .unwrap_or_default();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        if !raw.is_empty() {
            eprintln!("[s3-ipfs-adapter] config is not JSON ({e}); starting unconfigured");
        }
        serde_json::Value::Null
    });
    expand_env_refs(&mut v);
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    let mut prefix = s("prefix").unwrap_or_default();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    // The pin routes appear only when an `upload` object is configured.
    // uploadKey is the HMAC secret shared with the api-relay (reference a
    // deployment secret: "uploadKey": "$UPLOAD_KEY"); without it the routes
    // answer 503 unless allowUnsigned (dev/e2e) opens them.
    let upload = v.get("upload").and_then(|u| {
        if !u.is_object() {
            return None;
        }
        let us = |k: &str| u.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty());
        let un = |k: &str, d: u64| u.get(k).and_then(|x| x.as_u64()).unwrap_or(d);
        Some(upload::UploadCfg {
            upload_key: us("uploadKey").unwrap_or_default().to_string(),
            allow_unsigned: u.get("allowUnsigned").and_then(|x| x.as_bool()).unwrap_or(false),
            allow_origins: match u.get("allowOrigins").and_then(|x| x.as_array()) {
                Some(a) => a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.trim_end_matches('/').to_string())
                    .collect(),
                None => vec!["https://enclave.host".to_string()],
            },
            max_wasm: un("maxWasmBytes", 2 * 1024 * 1024 * 1024),
            max_image: un("maxImageBytes", 4 * 1024 * 1024) as usize,
            max_json: un("maxJsonBytes", 1024 * 1024) as usize,
            per_addr_daily: un("perAddrDailyBytes", 4 * 1024 * 1024 * 1024),
            global_daily: un("globalDailyBytes", 16 * 1024 * 1024 * 1024),
            json_per_ip_hourly: un("jsonPerIpHourly", 60) as f64,
        })
    });
    Config {
        title: s("title").unwrap_or_else(|| "S3 bucket over IPFS".to_string()),
        endpoint: s("endpoint").unwrap_or_default(),
        region: s("region").unwrap_or_else(|| "us-east-1".to_string()),
        bucket: s("bucket").unwrap_or_default(),
        prefix,
        creds: v.get("credentials").and_then(creds_from),
        refresh_secs: v.get("refreshSecs").and_then(|x| x.as_u64()).unwrap_or(300),
        max_keys: v
            .get("maxKeys")
            .and_then(|x| x.as_u64())
            .unwrap_or(50_000)
            .clamp(1, 500_000) as usize,
        api_key: s("api_key"),
        upload,
    }
}

impl Config {
    fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.endpoint.is_empty() {
            m.push("endpoint");
        }
        if self.bucket.is_empty() {
            m.push("bucket");
        }
        m
    }
}

/// Whether a request may trigger work (refresh). With no `api_key` the app
/// is open; with one set, present it as `Authorization: Bearer <key>`,
/// `X-Api-Key: <key>` or `?key=<key>`.
fn authorized(cfg: &Config, req: &Request) -> bool {
    let Some(want) = &cfg.api_key else { return true };
    if req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")) == Some(want) {
        return true;
    }
    if req.header("x-api-key") == Some(want.as_str()) {
        return true;
    }
    httpd::form_get(&req.query, "key").as_deref() == Some(want.as_str())
}

// ---- the index -------------------------------------------------------------

struct S3Ctx {
    ep: s3::Endpoint,
    bucket: String,
    creds: Option<s3::Creds>,
}

#[derive(Clone)]
struct FileEntry {
    key: String, // full S3 key
    rel: String, // path under the configured prefix
    size: u64,
    etag: String,
    leaves: Rc<Vec<[u8; 32]>>, // one sha2-256 per chunk, in order
    root: Cid,
    dag_size: u64,
}

/// An immutable, internally consistent view of the indexed bucket. Serving
/// paths clone the Rc; a refresh builds a new snapshot and swaps it, so
/// in-flight streams keep the world they started in.
struct Snapshot {
    files: Vec<FileEntry>,
    by_key: HashMap<String, u32>,
    nodes: HashMap<[u8; 32], Vec<u8>>, // dag-pb blocks by digest
    leaf_of: HashMap<[u8; 32], (u32, u32)>, // leaf digest -> (file, chunk)
    file_root: HashMap<[u8; 32], u32>, // file root digest -> file index
    root: Option<Cid>,
    total_bytes: u64,
}

impl Snapshot {
    fn empty() -> Snapshot {
        Snapshot {
            files: Vec::new(),
            by_key: HashMap::new(),
            nodes: HashMap::new(),
            leaf_of: HashMap::new(),
            file_root: HashMap::new(),
            root: None,
            total_bytes: 0,
        }
    }
}

/// Rebuild every derived structure from a set of file entries. All of it is
/// local CPU (protobuf + hashing a few bytes per chunk); no S3 involved.
fn commit(entries: Vec<FileEntry>) -> Snapshot {
    let mut snap = Snapshot::empty();
    for (fi, mut f) in entries.into_iter().enumerate() {
        let fi32 = fi as u32;
        let (root, dag_size, nodes) = ipfs::build_file_dag(&f.leaves, f.size);
        for n in nodes {
            snap.nodes.insert(n.cid.digest, n.block);
        }
        for (ci, d) in f.leaves.iter().enumerate() {
            snap.leaf_of.entry(*d).or_insert((fi32, ci as u32));
        }
        f.root = root;
        f.dag_size = dag_size;
        snap.file_root.entry(root.digest).or_insert(fi32);
        snap.total_bytes += f.size;
        snap.by_key.insert(f.key.clone(), fi32);
        snap.files.push(f);
    }
    // The directory tree. A key's path segments become nested UnixFS dirs;
    // when a file and a directory collide on a name (S3 allows "a" next to
    // "a/b"), the directory wins and the file is skipped with a log line.
    enum Node {
        Dir(BTreeMap<String, Node>),
        File(u32),
    }
    fn insert(m: &mut BTreeMap<String, Node>, segs: &[&str], fi: u32, key: &str) {
        if segs.len() == 1 {
            match m.entry(segs[0].to_string()) {
                std::collections::btree_map::Entry::Occupied(_) => {
                    eprintln!(
                        "[s3-ipfs-adapter] key {key} collides with an existing entry {}; skipped",
                        segs[0]
                    );
                }
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(Node::File(fi));
                }
            }
            return;
        }
        match m
            .entry(segs[0].to_string())
            .or_insert_with(|| Node::Dir(BTreeMap::new()))
        {
            Node::Dir(sub) => insert(sub, &segs[1..], fi, key),
            Node::File(_) => {
                eprintln!(
                    "[s3-ipfs-adapter] key {key} collides with a file at {}; skipped",
                    segs[0]
                );
            }
        }
    }
    let mut root = BTreeMap::new();
    for (fi, f) in snap.files.iter().enumerate() {
        let segs: Vec<&str> = f.rel.split('/').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            continue;
        }
        insert(&mut root, &segs, fi as u32, &f.key);
    }
    fn encode_dir(
        m: &BTreeMap<String, Node>,
        files: &[FileEntry],
        nodes: &mut HashMap<[u8; 32], Vec<u8>>,
    ) -> (Cid, u64) {
        let mut links = Vec::new();
        for (name, child) in m {
            let (cid, tsize) = match child {
                Node::Dir(sub) => encode_dir(sub, files, nodes),
                Node::File(fi) => {
                    let f = &files[*fi as usize];
                    (f.root, f.dag_size)
                }
            };
            links.push(ipfs::Link { cid, name: name.clone(), tsize });
        }
        let (cid, tsize, block) = ipfs::build_dir(links);
        if block.len() > 1024 * 1024 {
            eprintln!(
                "[s3-ipfs-adapter] warning: a directory block is {} KiB; kubo would HAMT-shard a directory this large, so its CID will differ from ipfs add's",
                block.len() / 1024
            );
        }
        nodes.insert(cid.digest, block);
        (cid, tsize)
    }
    let (root_cid, _) = encode_dir(&root, &snap.files, &mut snap.nodes);
    snap.root = Some(root_cid);
    snap
}

// ---- the indexer -----------------------------------------------------------

struct HashJob {
    meta: s3::ObjMeta,
    offset: u64,
    leaves: Vec<[u8; 32]>,
    attempts: u32, // failed tries for THIS object; skipped past MAX_ATTEMPTS
}

/// One persistently failing object must not wedge the whole index: after
/// this many failed hash attempts it is skipped until the next refresh.
const MAX_ATTEMPTS: u32 = 5;

enum Phase {
    Idle, // unconfigured
    // One GET of the persisted leaves cache before the first LIST, so a
    // restart re-hashes only what changed instead of the whole bucket.
    LoadCache { attempts: u32 },
    List { cont: Option<String>, acc: Vec<s3::ObjMeta> },
    Hash {
        queue: VecDeque<(s3::ObjMeta, u32)>, // (object, failed attempts so far)
        staged: Vec<FileEntry>,
        cur: Option<HashJob>,
        hashed: u64,
        to_hash: u64,
    },
    Ready,
}

struct Indexer {
    phase: Phase,
    last_list: Option<Instant>,
    truncated: bool,
    errors: u32,
    last_error: Option<String>,
    retry_at: Option<Instant>,
    skipped: u32, // objects given up on during the current/last hash cycle
}

impl Indexer {
    fn state_name(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "unconfigured",
            Phase::LoadCache { .. } => "loading-cache",
            Phase::List { .. } => "listing",
            Phase::Hash { .. } => "hashing",
            Phase::Ready => "ready",
        }
    }
}

struct App {
    cfg: Config,
    s3: Option<Rc<S3Ctx>>,
    snap: Rc<Snapshot>,
    indexer: Indexer,
    upload_shared: Rc<RefCell<upload::Shared>>,
    /// Uploads merged into the snapshot that no LIST has confirmed yet: a
    /// refresh that began before an upload landed would otherwise commit a
    /// snapshot WITHOUT it (its listing predates the object). Kept until a
    /// listing includes the key, unioned into every commit meanwhile.
    recent_uploads: Vec<FileEntry>,
    /// Parsed `_redirects` per root CID digest, `None` = that root has none.
    /// CIDs are immutable, so a parse is valid for that root forever — the
    /// cache never needs invalidating across reindexes.
    redirects_cache: HashMap<[u8; 32], Option<Rc<redirects::Redirects>>>,
    /// Rows recovered from the persisted leaves cache, keyed by object key.
    /// Consulted (behind the same (size, etag) binding as snapshot reuse)
    /// only until the first commit; cleared after.
    recovered: HashMap<String, leavescache::Row>,
    /// sha256 of the cache bytes last written (or None): saves are skipped
    /// while the committed rows would serialize to the same bytes.
    cache_fp: Option<[u8; 32]>,
}

impl App {
    fn fail(&mut self, err: String) {
        self.indexer.errors += 1;
        let backoff = (5 * u64::from(self.indexer.errors)).min(60);
        eprintln!(
            "[s3-ipfs-adapter] index error ({}, retry in {backoff}s): {err}",
            self.indexer.errors
        );
        self.indexer.last_error = Some(err);
        self.indexer.retry_at = Some(Instant::now() + Duration::from_secs(backoff));
    }

    /// One bounded unit of indexing work (at most one S3 request).
    /// Returns whether anything was done.
    fn tick(&mut self) -> bool {
        let Some(s3ctx) = self.s3.clone() else { return false };
        if let Some(t) = self.indexer.retry_at {
            if Instant::now() < t {
                return false;
            }
            self.indexer.retry_at = None;
        }
        enum After {
            Nothing,
            Worked,
            Fail(String),
            StartHash,
            Commit(Vec<FileEntry>),
            CacheLoaded(Vec<leavescache::Row>, [u8; 32]),
            CacheAbsent,
            CacheRetry(u32, String),
        }
        let prefix_len = self.cfg.prefix.len();
        let after = match &mut self.indexer.phase {
            Phase::Idle => After::Nothing,
            Phase::LoadCache { attempts } => {
                let tries = *attempts;
                let key = format!("{}{}", self.cfg.prefix, CACHE_REL);
                match s3::get_range(&s3ctx.ep, &s3ctx.bucket, &key, s3ctx.creds.as_ref(), 0, CACHE_MAX_BYTES) {
                    Ok(data) => match leavescache::deserialize(&data) {
                        // The loaded bytes' digest seeds the save fingerprint,
                        // so a boot that changes nothing re-writes nothing.
                        Ok(rows) => After::CacheLoaded(rows, Sha256::digest(&data).into()),
                        Err(e) => {
                            eprintln!("[s3-ipfs-adapter] leaves cache unusable ({e}); full re-hash");
                            After::CacheAbsent
                        }
                    },
                    Err(e) => {
                        // A first run has no cache object: nothing to reuse,
                        // not an error. Transient errors retry a few times —
                        // giving up too eagerly costs an 80-minute re-hash —
                        // but boot must never wedge on this: it is an
                        // optimization, the LIST+hash path is the truth.
                        if e.contains("404") || e.contains("NoSuchKey") {
                            After::CacheAbsent
                        } else if tries + 1 >= 3 {
                            eprintln!("[s3-ipfs-adapter] leaves cache unreadable after {} tries ({e}); full re-hash", tries + 1);
                            After::CacheAbsent
                        } else {
                            After::CacheRetry(tries + 1, format!("leaves cache: {e}"))
                        }
                    }
                }
            }
            Phase::Ready => {
                if self.cfg.refresh_secs > 0
                    && self
                        .indexer
                        .last_list
                        .is_none_or(|t| t.elapsed().as_secs() >= self.cfg.refresh_secs)
                {
                    self.indexer.phase = Phase::List { cont: None, acc: Vec::new() };
                    After::Worked
                } else {
                    After::Nothing
                }
            }
            Phase::List { cont, acc } => {
                let page = s3::list_page(
                    &s3ctx.ep,
                    &s3ctx.bucket,
                    &self.cfg.prefix,
                    cont.as_deref(),
                    s3ctx.creds.as_ref(),
                );
                match page {
                    Err(e) => After::Fail(format!("list: {e}")),
                    Ok((objs, next)) => {
                        for o in objs {
                            if o.key.ends_with('/') {
                                if o.size > 0 {
                                    eprintln!(
                                        "[s3-ipfs-adapter] skipping {} ({} bytes with a trailing slash)",
                                        o.key, o.size
                                    );
                                }
                                continue; // directory marker
                            }
                            // Derived state, not content: indexing the leaves
                            // cache would hash an object that changes on every
                            // save of the index it feeds.
                            if o.key[prefix_len.min(o.key.len())..].starts_with(CACHE_DIR_REL) {
                                continue;
                            }
                            acc.push(o);
                        }
                        self.indexer.errors = 0;
                        if acc.len() >= self.cfg.max_keys {
                            acc.truncate(self.cfg.max_keys);
                            self.indexer.truncated = true;
                            eprintln!(
                                "[s3-ipfs-adapter] listing capped at maxKeys={}; the index is PARTIAL",
                                self.cfg.max_keys
                            );
                            After::StartHash
                        } else if let Some(n) = next {
                            *cont = Some(n);
                            After::Worked
                        } else {
                            self.indexer.truncated = false;
                            After::StartHash
                        }
                    }
                }
            }
            Phase::Hash { queue, staged, cur, hashed, .. } => {
                if cur.is_none() {
                    match queue.pop_front() {
                        None => After::Commit(std::mem::take(staged)),
                        Some((meta, attempts)) => {
                            *cur = Some(HashJob { meta, offset: 0, leaves: Vec::new(), attempts });
                            After::Worked
                        }
                    }
                } else {
                    let job = cur.as_mut().unwrap();
                    let step: Result<(), String> = if job.meta.size == 0 {
                        job.leaves.push(Sha256::digest(b"").into());
                        Ok(())
                    } else {
                        let want = INDEX_WINDOW.min(job.meta.size - job.offset);
                        match s3::get_range(
                            &s3ctx.ep,
                            &s3ctx.bucket,
                            &job.meta.key,
                            s3ctx.creds.as_ref(),
                            job.offset,
                            want,
                        ) {
                            Err(e) => Err(format!("hash {}: {e}", job.meta.key)),
                            Ok(data) if data.len() as u64 != want => Err(format!(
                                "hash {}: short read ({} of {want} bytes; object changed?)",
                                job.meta.key,
                                data.len()
                            )),
                            Ok(data) => {
                                for chunk in data.chunks(CHUNK as usize) {
                                    job.leaves.push(Sha256::digest(chunk).into());
                                }
                                job.offset += want;
                                *hashed += want;
                                Ok(())
                            }
                        }
                    };
                    match step {
                        Err(e) => {
                            let job = cur.take().unwrap();
                            if job.attempts + 1 >= MAX_ATTEMPTS {
                                // Skip it; everything else still gets indexed.
                                eprintln!(
                                    "[s3-ipfs-adapter] skipping {} after {} failed attempts: {e}",
                                    job.meta.key,
                                    job.attempts + 1
                                );
                                self.indexer.last_error =
                                    Some(format!("skipped {}: {e}", job.meta.key));
                                self.indexer.skipped += 1;
                                After::Worked
                            } else {
                                // Requeue from scratch; fail() paces the retry.
                                queue.push_front((job.meta, job.attempts + 1));
                                After::Fail(e)
                            }
                        }
                        Ok(()) => {
                            if job.offset >= job.meta.size {
                                let job = cur.take().unwrap();
                                let rel = job.meta.key[prefix_len..].to_string();
                                staged.push(FileEntry {
                                    key: job.meta.key,
                                    rel,
                                    size: job.meta.size,
                                    etag: job.meta.etag,
                                    leaves: Rc::new(job.leaves),
                                    root: Cid::raw([0; 32]), // filled by commit()
                                    dag_size: 0,
                                });
                            }
                            After::Worked
                        }
                    }
                }
            }
        };
        match after {
            After::Nothing => false,
            After::Worked => true,
            After::Fail(e) => {
                self.fail(e);
                true
            }
            After::CacheLoaded(rows, fp) => {
                eprintln!("[s3-ipfs-adapter] leaves cache: {} rows", rows.len());
                self.recovered = rows.into_iter().map(|r| (r.key.clone(), r)).collect();
                self.cache_fp = Some(fp);
                self.indexer.phase = Phase::List { cont: None, acc: Vec::new() };
                true
            }
            After::CacheAbsent => {
                self.indexer.phase = Phase::List { cont: None, acc: Vec::new() };
                true
            }
            After::CacheRetry(attempts, e) => {
                self.indexer.phase = Phase::LoadCache { attempts };
                self.fail(e);
                true
            }
            After::StartHash => {
                self.start_hash();
                true
            }
            After::Commit(mut staged) => {
                // Union in uploads no listing has seen yet (see recent_uploads).
                for f in &self.recent_uploads {
                    if !staged.iter().any(|s| s.key == f.key) {
                        staged.push(f.clone());
                    }
                }
                let files = staged.len();
                let snap = commit(staged);
                eprintln!(
                    "[s3-ipfs-adapter] index ready: {} files, {} bytes, {} dag nodes, root {}",
                    files,
                    snap.total_bytes,
                    snap.nodes.len(),
                    snap.root.map(|c| c.to_string()).unwrap_or_default()
                );
                self.snap = Rc::new(snap);
                self.indexer.last_list = Some(Instant::now());
                self.indexer.last_error = (self.indexer.skipped > 0).then(|| {
                    format!(
                        "{} object(s) failed to hash and are missing from this index; see logs, then re-index",
                        self.indexer.skipped
                    )
                });
                self.indexer.phase = Phase::Ready;
                // From here reuse comes from the committed snapshot; holding
                // recovered rows past it would let a stale cache shadow later
                // listings if the two ever disagreed on an unchanged key.
                self.recovered = HashMap::new();
                self.save_leaves_cache();
                true
            }
        }
    }

    fn start_hash(&mut self) {
        let Phase::List { acc, .. } = std::mem::replace(&mut self.indexer.phase, Phase::Ready)
        else {
            return;
        };
        // A listing that names a recent upload has confirmed it: the normal
        // (size, etag) reuse below carries it from here on.
        self.recent_uploads
            .retain(|f| !acc.iter().any(|m| m.key == f.key));
        let mut staged = Vec::new();
        let mut queue = VecDeque::new();
        let mut to_hash = 0u64;
        let mut from_cache = 0usize;
        let prefix_len = self.cfg.prefix.len();
        for meta in acc {
            let reuse = self
                .snap
                .by_key
                .get(&meta.key)
                .map(|&i| &self.snap.files[i as usize])
                .filter(|f| f.size == meta.size && f.etag == meta.etag);
            match reuse {
                Some(f) => staged.push(f.clone()),
                // The persisted leaves cache answers exactly like snapshot
                // reuse, behind the same (size, etag) binding — this is what
                // turns a restart from a full re-hash into a LIST.
                None => match self
                    .recovered
                    .get(&meta.key)
                    .filter(|r| r.size == meta.size && r.etag == meta.etag)
                {
                    Some(r) => {
                        from_cache += 1;
                        staged.push(FileEntry {
                            rel: meta.key[prefix_len..].to_string(),
                            key: meta.key,
                            size: r.size,
                            etag: r.etag.clone(),
                            leaves: Rc::new(r.leaves.clone()),
                            root: Cid::raw([0; 32]), // filled by commit()
                            dag_size: 0,
                        });
                    }
                    None => {
                        to_hash += meta.size;
                        queue.push_back((meta, 0));
                    }
                },
            }
        }
        eprintln!(
            "[s3-ipfs-adapter] listed: {} unchanged ({from_cache} via the leaves cache), {} to hash ({} bytes)",
            staged.len(),
            queue.len(),
            to_hash
        );
        self.indexer.skipped = 0;
        self.indexer.phase = Phase::Hash { queue, staged, cur: None, hashed: 0, to_hash };
    }

    /// Persist the committed rows so the NEXT boot reuses them (see
    /// leavescache.rs). A failed save only costs that boot a re-hash: it
    /// logs and serving continues, and an unchanged snapshot writes nothing.
    fn save_leaves_cache(&mut self) {
        let Some(s3ctx) = self.s3.clone() else { return };
        let bytes = leavescache::serialize(
            self.snap.files.iter().map(|f| (f.key.as_str(), f.size, f.etag.as_str(), f.leaves.as_slice())),
        );
        let fp: [u8; 32] = Sha256::digest(&bytes).into();
        if self.cache_fp == Some(fp) {
            return;
        }
        let key = format!("{}{}", self.cfg.prefix, CACHE_REL);
        match s3::put_object(&s3ctx.ep, &s3ctx.bucket, &key, s3ctx.creds.as_ref(), &bytes) {
            Ok(()) => {
                self.cache_fp = Some(fp);
                eprintln!(
                    "[s3-ipfs-adapter] leaves cache saved: {} rows, {} bytes",
                    self.snap.files.len(),
                    bytes.len()
                );
            }
            Err(e) => eprintln!("[s3-ipfs-adapter] leaves cache save failed ({e}); a restart will re-hash"),
        }
    }
}

fn main() {
    println!("[s3-ipfs-adapter] {APP}");
    let cfg = load_config();
    let missing = cfg.missing();
    let s3ctx = if missing.is_empty() {
        match s3::Endpoint::parse(&cfg.endpoint, &cfg.region) {
            Ok(ep) => Some(Rc::new(S3Ctx {
                ep,
                bucket: cfg.bucket.clone(),
                creds: cfg.creds.clone(),
            })),
            Err(e) => {
                eprintln!("[s3-ipfs-adapter] bad endpoint: {e}; starting unconfigured");
                None
            }
        }
    } else {
        eprintln!(
            "[s3-ipfs-adapter] starting UNCONFIGURED: {} not set — set the deployment's config/secrets and restart",
            missing.join(", ")
        );
        None
    };
    match &cfg.creds {
        Some(c) => eprintln!(
            "[s3-ipfs-adapter] S3 requests will be signed as {}",
            c.access_key_id
        ),
        None => eprintln!(
            "[s3-ipfs-adapter] S3 requests will be UNSIGNED (no credentials resolved; public bucket assumed)"
        ),
    }
    let phase = if s3ctx.is_some() {
        Phase::LoadCache { attempts: 0 }
    } else {
        Phase::Idle
    };
    match &cfg.upload {
        Some(u) if u.enabled() => eprintln!(
            "[s3-ipfs-adapter] pin routes ON (/add-wasm /add-json /add-image), {}",
            if u.upload_key.is_empty() { "UNSIGNED (dev)" } else { "wallet-signed" }
        ),
        Some(_) => eprintln!(
            "[s3-ipfs-adapter] pin routes configured but uploadKey unresolved - they answer 503 until the secret is set"
        ),
        None => {}
    }
    let mut app = App {
        cfg,
        s3: s3ctx,
        snap: Rc::new(Snapshot::empty()),
        indexer: Indexer {
            phase,
            last_list: None,
            truncated: false,
            errors: 0,
            last_error: None,
            retry_at: None,
            skipped: 0,
        },
        upload_shared: upload::Shared::new(),
        recent_uploads: Vec::new(),
        redirects_cache: HashMap::new(),
        recovered: HashMap::new(),
        cache_fp: None,
    };
    let mut srv = Server::bind(APP, 8000);
    // Per-route buffered-body caps, enforced in the parser against
    // Content-Length before anything is buffered. The pin routes use their
    // CONFIGURED ceilings (defaults 1 MiB / 4 MiB) so an unauthenticated
    // request can never make the parser buffer more than that route allows;
    // /add-wasm is streamed, capped by the app in add_wasm().
    let json_cap = app.cfg.upload.as_ref().map_or(MAX_BODY, |u| u.max_json);
    let image_cap = app.cfg.upload.as_ref().map_or(MAX_BODY, |u| u.max_image);
    let big_routes: [(&str, usize); 3] = [
        ("/api/upload", MAX_UPLOAD),
        ("/add-json", json_cap),
        ("/add-image", image_cap),
    ];
    let mut tick: u64 = 0;
    loop {
        for (key, req) in srv.poll(MAX_BODY, &big_routes, "/add-wasm") {
            route(&mut app, &mut srv, key, &req);
        }
        merge_commits(&mut app);
        tick += 1;
        // Streams get the loop's blocking budget first; the indexer still
        // gets every 4th tick so an endless download cannot starve it.
        let pumped = srv.pump();
        let indexed = if !pumped || tick % 4 == 0 { app.tick() } else { false };
        merge_commits(&mut app);
        let flushed = srv.flush();
        if !(pumped || indexed || flushed) {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Fold completed uploads into the served snapshot so a freshly pinned CID
/// resolves immediately (a publish is typically followed by a deploy within
/// seconds; waiting for the next LIST would 404 it). The rebuild is skeleton
/// CPU only - no S3.
fn merge_commits(app: &mut App) {
    let pending = std::mem::take(&mut app.upload_shared.borrow_mut().commits);
    if pending.is_empty() {
        return;
    }
    let prefix_len = app.cfg.prefix.len();
    let mut entries: Vec<FileEntry> = app.snap.files.clone();
    let mut changed = false;
    for c in pending {
        if entries.iter().any(|f| f.key == c.key) {
            continue; // duplicate pin of an already-indexed object
        }
        let entry = FileEntry {
            rel: c.key[prefix_len..].to_string(),
            key: c.key,
            size: c.size,
            etag: c.etag,
            leaves: Rc::new(c.leaves),
            root: Cid::raw([0; 32]), // filled by commit()
            dag_size: 0,
        };
        app.recent_uploads.push(entry.clone());
        entries.push(entry);
        changed = true;
    }
    if changed {
        app.snap = Rc::new(commit(entries));
        // A pin followed by a restart must survive it: persist right away
        // rather than waiting for the next scheduled LIST to confirm it.
        app.save_leaves_cache();
    }
}

// ---- routing ---------------------------------------------------------------

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => srv.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "no-store")
                .with(
                    "content-security-policy",
                    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'",
                )
                .with("referrer-policy", "no-referrer")
                .with("x-content-type-options", "nosniff")
                .body("text/html; charset=utf-8", UI),
        ),
        ("GET", "/ping") => srv.respond(key, Response::new(200, "OK").body("text/plain", "ok\n")),
        // The upload gateway's liveness probe, kept verbatim: deploy tooling
        // and the gateway test suites wait on it.
        ("GET", "/healthz") => srv.respond(key, json(200, "OK", "{\"ok\":true}".into())),
        ("POST", "/add-wasm") => upload::add_wasm(app, srv, key, req),
        ("POST", "/add-json") => upload::add_json(app, srv, key, req),
        ("POST", "/add-image") => upload::add_image(app, srv, key, req),
        ("OPTIONS", "/add-wasm") | ("OPTIONS", "/add-json") | ("OPTIONS", "/add-image") => {
            let resp = Response::new(204, "No Content");
            let resp = match &app.cfg.upload {
                Some(cfg) => upload::cors(resp, req, cfg),
                None => resp,
            };
            srv.respond(key, resp)
        }
        ("GET", "/api/status") => srv.respond(key, api_status(app, srv.uptime_secs())),
        ("GET", "/api/files") => api_files(app, srv, key),
        ("POST", "/api/refresh") => api_refresh(app, srv, key, req),
        ("POST", "/api/upload") => api_upload(app, srv, key, req),
        ("POST", "/api/delete") => api_delete(app, srv, key, req),
        ("GET", p) | ("HEAD", p) if p == "/ipfs" || p.starts_with("/ipfs/") => {
            gateway(app, srv, key, req)
        }
        _ => srv.respond(key, json(404, "Not Found", "{\"error\":\"no such route\"}".into())),
    }
}

fn api_status(app: &App, uptime: u64) -> Response {
    let idx = &app.indexer;
    let snap = &app.snap;
    let mut extra = String::new();
    match &idx.phase {
        Phase::List { acc, .. } => {
            extra = format!(",\"listed\":{}", acc.len());
        }
        Phase::Hash { queue, staged, hashed, to_hash, cur } => {
            extra = format!(
                ",\"unchanged\":{},\"queue\":{},\"hashedBytes\":{},\"toHashBytes\":{},\"hashing\":\"{}\"",
                staged.len(),
                queue.len(),
                hashed,
                to_hash,
                json_escape(cur.as_ref().map(|j| j.meta.key.as_str()).unwrap_or(""))
            );
        }
        _ => {}
    }
    let missing: Vec<String> = app.cfg.missing().iter().map(|m| format!("\"{m}\"")).collect();
    json(
        200,
        "OK",
        format!(
            "{{\"app\":\"{}\",\"state\":\"{}\",\"uptime\":{uptime},\"title\":\"{}\",\"bucket\":\"{}\",\"prefix\":\"{}\",\"files\":{},\"totalBytes\":{},\"dagNodes\":{},\"rootCid\":{},\"truncated\":{},\"refreshSecs\":{},\"lastIndexAge\":{},\"missing\":[{}],\"error\":{}{extra}}}",
            APP,
            app.indexer.state_name(),
            json_escape(&app.cfg.title),
            json_escape(&app.cfg.bucket),
            json_escape(&app.cfg.prefix),
            snap.files.len(),
            snap.total_bytes,
            snap.nodes.len(),
            snap.root
                .map(|c| format!("\"{c}\""))
                .unwrap_or_else(|| "null".into()),
            idx.truncated,
            app.cfg.refresh_secs,
            idx.last_list
                .map(|t| t.elapsed().as_secs().to_string())
                .unwrap_or_else(|| "null".into()),
            missing.join(","),
            idx.last_error
                .as_ref()
                .map(|e| format!("\"{}\"", json_escape(e)))
                .unwrap_or_else(|| "null".into()),
        ),
    )
}

fn api_files(app: &App, srv: &mut Server, key: usize) {
    let snap = &app.snap;
    let mut out = String::with_capacity(snap.files.len() * 120 + 2);
    out.push('[');
    for (i, f) in snap.files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"size\":{},\"cid\":\"{}\"}}",
            json_escape(&f.rel),
            f.size,
            f.root
        ));
    }
    out.push(']');
    respond_sized(
        srv,
        key,
        json(200, "OK", out),
        false,
    );
}

fn api_refresh(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    if !authorized(&app.cfg, req) {
        return srv.respond(key, json(403, "Forbidden", "{\"error\":\"bad key\"}".into()));
    }
    if app.s3.is_none() {
        return srv.respond(key, json(503, "Service Unavailable", "{\"error\":\"unconfigured\"}".into()));
    }
    restart_listing(app);
    srv.respond(key, json(200, "OK", "{\"ok\":true,\"note\":\"listing restarted\"}".into()))
}

/// Restart listing from ANY phase: a refresh must always visibly do its
/// job. Dropping in-flight staging is safe (serving runs from the
/// committed snapshot), and it is also the way out of a wedged hash job.
fn restart_listing(app: &mut App) {
    app.indexer.phase = Phase::List { cont: None, acc: Vec::new() };
    app.indexer.retry_at = None;
    app.indexer.errors = 0;
}

/// `k=v` lookup with percent-decoding ONLY: object keys and paths keep
/// their literal '+' (the space alias is a form convention that browsers'
/// encodeURIComponent never produces).
fn raw_get(pairs: &str, key: &str) -> Option<String> {
    for pair in pairs.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k == key {
            return httpd::percent_decode(v);
        }
    }
    None
}

/// A relative object key as accepted from the UI: non-empty, no leading or
/// trailing slash, no empty or dot-only segments, no control bytes, and
/// short enough for S3's 1024-byte key limit once the prefix is prepended.
fn valid_rel_key(k: &str, prefix: &str) -> bool {
    !k.is_empty()
        && k.len() + prefix.len() <= 1024
        && !k.starts_with('/')
        && !k.ends_with('/')
        && !k.bytes().any(|b| b < 0x20 || b == 0x7f)
        && k.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

fn api_upload(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    if !authorized(&app.cfg, req) {
        return srv.respond(key, json(403, "Forbidden", "{\"error\":\"bad key\"}".into()));
    }
    let Some(s3ctx) = app.s3.clone() else {
        return srv.respond(key, json(503, "Service Unavailable", "{\"error\":\"unconfigured\"}".into()));
    };
    // ?path= names the object; ?key= stays reserved for the api key.
    let Some(rel) = raw_get(&req.query, "path").filter(|k| valid_rel_key(k, &app.cfg.prefix))
    else {
        return srv.respond(key, json(400, "Bad Request", "{\"error\":\"bad or missing ?path=\"}".into()));
    };
    let full = format!("{}{rel}", app.cfg.prefix);
    match s3::put_object(&s3ctx.ep, &s3ctx.bucket, &full, s3ctx.creds.as_ref(), &req.body) {
        Ok(()) => {
            eprintln!(
                "[s3-ipfs-adapter] uploaded {} ({} bytes); re-listing",
                full,
                req.body.len()
            );
            restart_listing(app);
            srv.respond(
                key,
                json(200, "OK", format!("{{\"ok\":true,\"path\":\"{}\"}}", json_escape(&rel))),
            )
        }
        Err(e) => srv.respond(
            key,
            json(502, "Bad Gateway", format!("{{\"error\":\"{}\"}}", json_escape(&e))),
        ),
    }
}

fn api_delete(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    if !authorized(&app.cfg, req) {
        return srv.respond(key, json(403, "Forbidden", "{\"error\":\"bad key\"}".into()));
    }
    let Some(s3ctx) = app.s3.clone() else {
        return srv.respond(key, json(503, "Service Unavailable", "{\"error\":\"unconfigured\"}".into()));
    };
    let body = String::from_utf8_lossy(&req.body).to_string();
    let Some(rel) = raw_get(&body, "path").filter(|k| valid_rel_key(k, &app.cfg.prefix)) else {
        return srv.respond(key, json(400, "Bad Request", "{\"error\":\"bad or missing path=\"}".into()));
    };
    let full = format!("{}{rel}", app.cfg.prefix);
    match s3::delete_object(&s3ctx.ep, &s3ctx.bucket, &full, s3ctx.creds.as_ref()) {
        Ok(()) => {
            eprintln!("[s3-ipfs-adapter] deleted {full}; re-listing");
            restart_listing(app);
            srv.respond(key, json(200, "OK", "{\"ok\":true}".into()))
        }
        Err(e) => srv.respond(
            key,
            json(502, "Bad Gateway", format!("{{\"error\":\"{}\"}}", json_escape(&e))),
        ),
    }
}

/// Send a buffered Response, spilling into a stream when it is larger than
/// the engine's slow-client allowance. HEAD gets the same header block
/// (including the true content-length) and no body.
fn respond_sized(srv: &mut Server, key: usize, resp: Response, head_only: bool) {
    if !head_only && resp.body.len() <= DIRECT_MAX {
        return srv.respond(key, resp);
    }
    let Response { status, reason, headers, body } = resp;
    let len = body.len() as u64;
    let mut r = Response::new(status, reason);
    r.headers = headers;
    srv.respond_stream(key, r, Some(len), head_only, Box::new(VecBody { data: body, pos: 0 }));
}

// ---- gateway ---------------------------------------------------------------

enum Format {
    Unixfs,
    Raw,
    Car,
}

fn gateway(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let head_only = req.method == "HEAD";
    if app.s3.is_none() {
        return srv.respond(key, json(503, "Service Unavailable", "{\"error\":\"unconfigured\"}".into()));
    }
    let rest = req.path.strip_prefix("/ipfs").unwrap_or("");
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let (cid_s, subpath) = match rest.split_once('/') {
        Some((c, p)) => (c, p),
        None => (rest, ""),
    };
    let Some(root_cid) = Cid::parse(cid_s) else {
        return srv.respond(
            key,
            json(400, "Bad Request", "{\"error\":\"not a CIDv1 this gateway mints (base32, sha2-256, raw or dag-pb)\"}".into()),
        );
    };
    let snap = app.snap.clone();
    let s3ctx = app.s3.as_ref().unwrap().clone();

    // Format negotiation: explicit ?format= wins, then Accept. Computed before
    // the walk so a miss can tell a web (UnixFS) request — which falls through
    // to _redirects — from a trustless ?format=raw/car one, whose miss is a 404.
    let accept = req.header("accept").unwrap_or("");
    let format = match httpd::form_get(&req.query, "format").as_deref() {
        Some("raw") => Format::Raw,
        Some("car") => Format::Car,
        Some(_) => Format::Unixfs,
        None if accept.contains("application/vnd.ipld.raw") => Format::Raw,
        None if accept.contains("application/vnd.ipld.car") => Format::Car,
        None => Format::Unixfs,
    };

    // Walk the sub-path through UnixFS directories.
    let mut cur = root_cid;
    let mut walked: Vec<String> = Vec::new();
    let mut absent = false;
    for seg in subpath.split('/').filter(|s| !s.is_empty()) {
        if cur.codec != CODEC_DAG_PB {
            absent = true; // the path tries to descend into a leaf
            break;
        }
        let Some(block) = snap.nodes.get(&cur.digest) else {
            return srv.respond(key, not_indexed(&cur));
        };
        let Some((links, data)) = ipfs::dagpb_decode(block) else {
            return srv.respond(key, json(500, "Internal Server Error", "{\"error\":\"bad node\"}".into()));
        };
        if !ipfs::is_unixfs_dir(&data) {
            absent = true; // a segment descends through a file
            break;
        }
        let Some(link) = links.iter().find(|l| l.name == seg) else {
            absent = true; // no such entry
            break;
        };
        cur = link.cid;
        walked.push(seg.to_string());
    }

    // A path absent from the DAG falls through to the site root's _redirects
    // (pretty URLs, the branded 404) — but only for a web (UnixFS) request.
    if absent {
        if matches!(format, Format::Unixfs)
            && try_redirects(app, &snap, &s3ctx, srv, key, req, &root_cid, subpath, head_only)
        {
            return;
        }
        return srv.respond(key, json(404, "Not Found", "{\"error\":\"no such path\"}".into()));
    }

    let base = gateway_headers(&root_cid, &cur, req);
    match format {
        Format::Raw => serve_raw(&snap, &s3ctx, srv, key, &cur, base, head_only),
        Format::Car => {
            let name = format!("{cur}.car");
            let resp = base
                .with("content-type", "application/vnd.ipld.car; version=1")
                .with("content-disposition", &format!("attachment; filename=\"{name}\""));
            let src = CarBody::new(snap, s3ctx, cur);
            srv.respond_stream(key, resp, None, head_only, Box::new(src));
        }
        Format::Unixfs => serve_unixfs(&snap, &s3ctx, srv, key, req, &root_cid, &cur, &walked, head_only),
    }
}

/// Consult the site root's `_redirects` for a path that missed the DAG. Returns
/// true if it produced a response (a 200 rewrite, a non-200 page such as the
/// branded 404, or a 3xx redirect); false if no rule matched, so the caller
/// falls back to a plain 404.
fn try_redirects(
    app: &mut App,
    snap: &Rc<Snapshot>,
    s3ctx: &Rc<S3Ctx>,
    srv: &mut Server,
    key: usize,
    req: &Request,
    root_cid: &Cid,
    subpath: &str,
    head_only: bool,
) -> bool {
    let Some(rules) = redirects_for(app, snap, s3ctx, root_cid) else {
        return false;
    };
    let Some(m) = rules.lookup(&format!("/{subpath}")) else {
        return false;
    };
    // A 3xx rule is a redirect, not a content rewrite.
    if (300..400).contains(&m.status) {
        srv.respond(
            key,
            Response::new(m.status, status_reason(m.status))
                .with("location", &m.to)
                .with("cache-control", "no-store"),
        );
        return true;
    }
    // Resolve the target within the same root. If it too is absent, don't
    // loop — let the caller emit a plain 404.
    let to_sub = m.to.trim_start_matches('/');
    let Some(target) = walk_to(snap, root_cid, to_sub) else {
        return false;
    };
    if m.status == 200 {
        // Serve it exactly as a direct request for the target would: streamed,
        // right content-type, 200 + ETag.
        let walked: Vec<String> =
            to_sub.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
        serve_unixfs(snap, s3ctx, srv, key, req, root_cid, &target, &walked, head_only);
        return true;
    }
    // A non-200 rewrite (e.g. the `/* /404.html 404` catch-all): serve the
    // target's bytes verbatim, but carrying the rule's status.
    let Some(bytes) = read_small_file(snap, s3ctx, &target) else {
        return false;
    };
    let mut resp = Response::new(m.status, status_reason(m.status))
        .with("content-type", content_type_for(to_sub))
        .with("cache-control", "no-store")
        .with("x-content-type-options", "nosniff");
    resp.body = bytes;
    respond_sized(srv, key, resp, head_only);
    true
}

/// Parsed `_redirects` for a root, from cache or a one-time DAG read. `None`
/// (also cached) when the root has no readable/parseable/non-empty `_redirects`.
/// Root CIDs are immutable, so a cached parse is valid for that root forever.
fn redirects_for(
    app: &mut App,
    snap: &Rc<Snapshot>,
    s3ctx: &Rc<S3Ctx>,
    root_cid: &Cid,
) -> Option<Rc<redirects::Redirects>> {
    if let Some(hit) = app.redirects_cache.get(&root_cid.digest) {
        return hit.clone();
    }
    let parsed = walk_to(snap, root_cid, "_redirects")
        .and_then(|cid| read_small_file(snap, s3ctx, &cid))
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|text| Rc::new(redirects::parse(&text)))
        .filter(|r| !r.is_empty());
    app.redirects_cache.insert(root_cid.digest, parsed.clone());
    parsed
}

/// Walk a sub-path to its CID without serving. `None` on any miss.
fn walk_to(snap: &Snapshot, root_cid: &Cid, subpath: &str) -> Option<Cid> {
    let mut cur = *root_cid;
    for seg in subpath.split('/').filter(|s| !s.is_empty()) {
        if cur.codec != CODEC_DAG_PB {
            return None;
        }
        let block = snap.nodes.get(&cur.digest)?;
        let (links, data) = ipfs::dagpb_decode(block)?;
        if !ipfs::is_unixfs_dir(&data) {
            return None;
        }
        cur = links.iter().find(|l| l.name == seg)?.cid;
    }
    Some(cur)
}

/// Read a small whole file (a single raw leaf, or a chunked file up to a few
/// MiB) from the DAG — used only for `_redirects` and redirect targets, which
/// are tiny HTML/text. Larger targets return `None` (caller 404s).
fn read_small_file(snap: &Snapshot, s3ctx: &S3Ctx, cid: &Cid) -> Option<Vec<u8>> {
    const CAP: u64 = 4 << 20;
    if cid.codec == CODEC_RAW {
        let &(fi, ci) = snap.leaf_of.get(&cid.digest)?;
        return fetch_leaf(snap, s3ctx, fi, ci).ok();
    }
    let &fi = snap.file_root.get(&cid.digest)?;
    let f = &snap.files[fi as usize];
    if f.size > CAP {
        return None;
    }
    let mut out = Vec::with_capacity(f.size as usize);
    for ci in 0..f.leaves.len() as u32 {
        out.extend(fetch_leaf(snap, s3ctx, fi, ci).ok()?);
    }
    Some(out)
}

/// A static reason phrase for the statuses `_redirects` can produce.
fn status_reason(status: u16) -> &'static str {
    match status {
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        404 => "Not Found",
        410 => "Gone",
        451 => "Unavailable For Legal Reasons",
        _ => "OK",
    }
}

fn not_indexed(cid: &Cid) -> Response {
    json(
        404,
        "Not Found",
        format!(
            "{{\"error\":\"CID {} is not in this gateway's index (it only serves the configured bucket)\"}}",
            cid
        ),
    )
}

fn gateway_headers(root: &Cid, cur: &Cid, req: &Request) -> Response {
    Response::new(200, "OK")
        .with("etag", &format!("\"{cur}\""))
        .with("cache-control", "public, max-age=29030400, immutable")
        .with("x-ipfs-path", &req.path)
        .with("x-ipfs-roots", &root.to_string())
        .with("x-content-type-options", "nosniff")
        // Pinned bytes include publisher SVGs: sandbox kills script on a
        // direct navigation (image contexts never ran it) - the second layer
        // behind the /add-image validator. Caddy used to add this in front
        // of Kubo; there is no Caddy in front of this app.
        .with(
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; sandbox",
        )
}

/// One verified block, as application/vnd.ipld.raw.
fn serve_raw(
    snap: &Rc<Snapshot>,
    s3ctx: &Rc<S3Ctx>,
    srv: &mut Server,
    key: usize,
    cid: &Cid,
    base: Response,
    head_only: bool,
) {
    let resp = base.with("content-type", "application/vnd.ipld.raw");
    if cid.codec == CODEC_DAG_PB {
        let Some(block) = snap.nodes.get(&cid.digest) else {
            return srv.respond(key, not_indexed(cid));
        };
        let mut resp = resp;
        resp.body = block.clone();
        return respond_sized(srv, key, resp, head_only);
    }
    let Some(&(fi, ci)) = snap.leaf_of.get(&cid.digest) else {
        return srv.respond(key, not_indexed(cid));
    };
    if head_only {
        let f = &snap.files[fi as usize];
        let len = ipfs::chunk_sizes(f.size)[ci as usize];
        let Response { status, reason, headers, .. } = resp;
        let mut r = Response::new(status, reason);
        r.headers = headers;
        return srv.respond_stream(key, r, Some(len), true, Box::new(VecBody { data: Vec::new(), pos: 0 }));
    }
    match fetch_leaf(snap, s3ctx, fi, ci) {
        Ok(bytes) => {
            let mut resp = resp;
            resp.body = bytes;
            respond_sized(srv, key, resp, false)
        }
        Err(e) => srv.respond(
            key,
            json(502, "Bad Gateway", format!("{{\"error\":\"{}\"}}", json_escape(&e))),
        ),
    }
}

/// Fetch and hash-verify one leaf chunk from S3.
fn fetch_leaf(snap: &Snapshot, s3ctx: &S3Ctx, fi: u32, ci: u32) -> Result<Vec<u8>, String> {
    let f = &snap.files[fi as usize];
    let sizes = ipfs::chunk_sizes(f.size);
    let len = sizes[ci as usize];
    let start = u64::from(ci) * CHUNK;
    let data = if len == 0 {
        Vec::new()
    } else {
        s3::get_range(&s3ctx.ep, &s3ctx.bucket, &f.key, s3ctx.creds.as_ref(), start, len)?
    };
    if data.len() as u64 != len {
        return Err(format!("short read from S3 for {} (object changed?)", f.key));
    }
    let digest: [u8; 32] = Sha256::digest(&data).into();
    if digest != f.leaves[ci as usize] {
        return Err(format!(
            "object {} changed in S3 since indexing (hash mismatch); POST /api/refresh",
            f.key
        ));
    }
    Ok(data)
}

#[allow(clippy::too_many_arguments)]
fn serve_unixfs(
    snap: &Rc<Snapshot>,
    s3ctx: &Rc<S3Ctx>,
    srv: &mut Server,
    key: usize,
    req: &Request,
    root: &Cid,
    cur: &Cid,
    walked: &[String],
    head_only: bool,
) {
    // A raw CID is file content directly (a whole small file or one chunk).
    if cur.codec == CODEC_RAW {
        let Some(&(fi, ci)) = snap.leaf_of.get(&cur.digest) else {
            return srv.respond(key, not_indexed(cur));
        };
        let f = &snap.files[fi as usize];
        let start = u64::from(ci) * CHUNK;
        let len = ipfs::chunk_sizes(f.size)[ci as usize];
        let name = walked
            .last()
            .cloned()
            .or_else(|| snap.file_root.get(&cur.digest).map(|&i| leaf_name(snap, i)));
        return serve_span(snap, s3ctx, srv, key, req, root, cur, fi, start, len, name, head_only);
    }
    let Some(block) = snap.nodes.get(&cur.digest) else {
        return srv.respond(key, not_indexed(cur));
    };
    let Some((links, data)) = ipfs::dagpb_decode(block) else {
        return srv.respond(key, json(500, "Internal Server Error", "{\"error\":\"bad node\"}".into()));
    };
    if ipfs::is_unixfs_dir(&data) {
        // index.html wins, else an HTML listing.
        if let Some(ix) = links.iter().find(|l| l.name == "index.html") {
            let mut walked2 = walked.to_vec();
            walked2.push("index.html".into());
            return serve_unixfs(snap, s3ctx, srv, key, req, root, &ix.cid, &walked2, head_only);
        }
        let resp = gateway_headers(root, cur, req)
            .with("content-type", "text/html; charset=utf-8");
        let mut resp = resp;
        resp.body = dir_listing_html(root, cur, walked, &links).into_bytes();
        return respond_sized(srv, key, resp, head_only);
    }
    // A dag-pb UnixFS file node: its content is a contiguous chunk span of
    // some indexed object. filesize comes from the node.
    let Some(filesize) = ipfs::unixfs_file_size(&data) else {
        return srv.respond(key, json(500, "Internal Server Error", "{\"error\":\"unsupported unixfs node\"}".into()));
    };
    let name = walked
        .last()
        .cloned()
        .or_else(|| snap.file_root.get(&cur.digest).map(|&i| leaf_name(snap, i)));
    // A file ROOT resolves through file_root: exact by construction. The
    // leftmost-leaf shortcut below is only for interior nodes, and it can
    // pick the WRONG file when two objects share their leading chunk bytes
    // (leaf_of keeps the first writer) - e.g. a file and an extended copy of
    // it. Trusting it unchecked served a span past the shorter file's end,
    // which was an out-of-bounds panic in FileBody - one hostile pair of
    // uploads away from killing the process. Roots never hit that; interior
    // spans are bounds-checked against the file they landed on.
    if let Some(&fi) = snap.file_root.get(&cur.digest) {
        return serve_span(snap, s3ctx, srv, key, req, root, cur, fi, 0, filesize, name, head_only);
    }
    let mut first = *cur;
    let mut guard = 0;
    while first.codec == CODEC_DAG_PB {
        guard += 1;
        if guard > 16 {
            return srv.respond(key, json(500, "Internal Server Error", "{\"error\":\"dag too deep\"}".into()));
        }
        let Some(b) = snap.nodes.get(&first.digest) else {
            return srv.respond(key, not_indexed(&first));
        };
        let Some((ls, _)) = ipfs::dagpb_decode(b) else {
            return srv.respond(key, json(500, "Internal Server Error", "{\"error\":\"bad node\"}".into()));
        };
        let Some(l0) = ls.first() else {
            return srv.respond(key, json(500, "Internal Server Error", "{\"error\":\"empty file node\"}".into()));
        };
        first = l0.cid;
    }
    let Some(&(fi, ci)) = snap.leaf_of.get(&first.digest) else {
        return srv.respond(key, not_indexed(&first));
    };
    let start = u64::from(ci) * CHUNK;
    if start + filesize > snap.files[fi as usize].size {
        // The leaf run continues in a different (longer) object than the one
        // leaf_of recorded. Serving it as a byte span is not possible from
        // this index; the CAR/raw forms of the same content still work.
        return srv.respond(key, json(404, "Not Found", "{\"error\":\"this node spans an object the index maps elsewhere; fetch it as ?format=car\"}".into()));
    }
    serve_span(snap, s3ctx, srv, key, req, root, cur, fi, start, filesize, name, head_only)
}

fn leaf_name(snap: &Snapshot, fi: u32) -> String {
    let rel = &snap.files[fi as usize].rel;
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

/// Serve `len` bytes of file `fi` starting at content offset `start`, with
/// HTTP range support.
#[allow(clippy::too_many_arguments)]
fn serve_span(
    snap: &Rc<Snapshot>,
    s3ctx: &Rc<S3Ctx>,
    srv: &mut Server,
    key: usize,
    req: &Request,
    root: &Cid,
    cur: &Cid,
    fi: u32,
    start: u64,
    len: u64,
    name: Option<String>,
    head_only: bool,
) {
    let mut resp = gateway_headers(root, cur, req).with("accept-ranges", "bytes");
    let filename = httpd::form_get(&req.query, "filename").or(name);
    let ct = filename
        .as_deref()
        .map(content_type_for)
        .unwrap_or("application/octet-stream");
    resp = resp.with("content-type", ct);
    if httpd::form_get(&req.query, "download").is_some() {
        let n = filename.unwrap_or_else(|| cur.to_string());
        resp = resp.with(
            "content-disposition",
            &format!("attachment; filename=\"{}\"", n.replace('"', "")),
        );
    }
    // One absolute byte range, per RFC 9110; anything else gets the whole file.
    let (a, b) = match req.header("range").and_then(|r| parse_range(r, len)) {
        Some(Err(())) => {
            let resp = Response::new(416, "Range Not Satisfiable")
                .with("content-range", &format!("bytes */{len}"));
            return srv.respond(key, resp);
        }
        Some(Ok((a, b))) => {
            resp.status = 206;
            resp.reason = "Partial Content";
            resp = resp.with("content-range", &format!("bytes {a}-{b}/{len}"));
            (a, b)
        }
        None => {
            if len == 0 {
                return respond_sized(srv, key, resp, head_only);
            }
            (0, len - 1)
        }
    };
    let src = FileBody {
        snap: snap.clone(),
        s3: s3ctx.clone(),
        file: fi,
        pos: start + a,
        end: start + b + 1,
    };
    srv.respond_stream(key, resp, Some(b - a + 1), head_only, Box::new(src));
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` against a body of `len` bytes.
/// None = no/unsupported range (serve 200); Some(Err) = unsatisfiable.
fn parse_range(header: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multi-range: serve the whole body instead
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        // suffix: last n bytes
        let n: u64 = b.parse().ok()?;
        if n == 0 || len == 0 {
            return Some(Err(()));
        }
        let n = n.min(len);
        return Some(Ok((len - n, len - 1)));
    }
    let start: u64 = a.parse().ok()?;
    if start >= len {
        return Some(Err(()));
    }
    let end = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().ok()?.min(len - 1)
    };
    if end < start {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

fn content_type_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" | "log" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "zip" => "application/zip",
        "wasm" => "application/wasm",
        "car" => "application/vnd.ipld.car",
        "csv" => "text/csv; charset=utf-8",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Percent-encode one path segment for use in an href.
fn enc_seg(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn human_size(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.2} GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.2} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn dir_listing_html(root: &Cid, cur: &Cid, walked: &[String], links: &[ipfs::Link]) -> String {
    let base: String = {
        let mut s = format!("/ipfs/{root}");
        for seg in walked {
            s.push('/');
            s.push_str(&enc_seg(seg));
        }
        s
    };
    let crumb = {
        let mut c = format!("<a href=\"/ipfs/{root}\">{root_short}</a>", root_short = short_cid(&root.to_string()));
        let mut acc = format!("/ipfs/{root}");
        for seg in walked {
            acc.push('/');
            acc.push_str(&enc_seg(seg));
            c.push_str(&format!(" / <a href=\"{acc}\">{}</a>", html_escape(seg)));
        }
        c
    };
    let mut rows = String::new();
    for l in links {
        let href = format!("{base}/{}", enc_seg(&l.name));
        rows.push_str(&format!(
            "<tr><td><a href=\"{href}\">{}</a></td><td>{}</td><td class=c><a href=\"/ipfs/{cid}\">{cid_s}</a></td></tr>",
            html_escape(&l.name),
            human_size(l.tsize),
            cid = l.cid,
            cid_s = short_cid(&l.cid.to_string()),
        ));
    }
    format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title><style>body{{font:14px/1.6 ui-monospace,monospace;background:#0b0e14;color:#d7dde8;margin:2em auto;max-width:860px;padding:0 1em}}a{{color:#e8a34c;text-decoration:none}}a:hover{{text-decoration:underline}}table{{border-collapse:collapse;width:100%;margin-top:1em}}td{{padding:4px 10px 4px 0;border-bottom:1px solid #1e2430}}.c{{color:#8a94a6}}h1{{font-size:16px}}</style><h1>Index of {crumb}</h1><table>{rows}</table><p class=c>UnixFS directory {cur} — served by {app}</p>",
        title = html_escape(&format!("Index of {}", walked.last().map(String::as_str).unwrap_or("/"))),
        app = APP,
    )
}

fn short_cid(c: &str) -> String {
    if c.len() > 16 {
        format!("{}…{}", &c[..8], &c[c.len() - 6..])
    } else {
        c.to_string()
    }
}

// ---- streaming bodies ------------------------------------------------------

struct VecBody {
    data: Vec<u8>,
    pos: usize,
}

impl Body for VecBody {
    fn pull(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let end = (self.pos + 128 * 1024).min(self.data.len());
        let out = self.data[self.pos..end].to_vec();
        self.pos = end;
        Ok(Some(out))
    }
}

/// Streams `[pos, end)` of a file's content, fetching whole chunks from S3
/// (one ranged request per pull) and verifying each against the index.
struct FileBody {
    snap: Rc<Snapshot>,
    s3: Rc<S3Ctx>,
    file: u32,
    pos: u64, // next content byte to emit
    end: u64, // exclusive
}

impl Body for FileBody {
    fn pull(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.pos >= self.end {
            return Ok(None);
        }
        let f = &self.snap.files[self.file as usize];
        // Belt over the router's braces: a span outside the file must be a
        // clean truncation error, never a slice panic (one panic is the
        // whole process).
        if self.end > f.size {
            return Err(format!(
                "span [{}, {}) exceeds {} ({} bytes)",
                self.pos, self.end, f.key, f.size
            ));
        }
        let chunk0 = self.pos / CHUNK;
        let last_needed = (self.end - 1) / CHUNK;
        let max_chunks = (STREAM_WINDOW / CHUNK).max(1);
        let chunk_last = last_needed.min(chunk0 + max_chunks - 1);
        let fetch_start = chunk0 * CHUNK;
        let fetch_end = ((chunk_last + 1) * CHUNK).min(f.size);
        let data = s3::get_range(
            &self.s3.ep,
            &self.s3.bucket,
            &f.key,
            self.s3.creds.as_ref(),
            fetch_start,
            fetch_end - fetch_start,
        )?;
        if data.len() as u64 != fetch_end - fetch_start {
            return Err(format!("short read from S3 for {} (object changed?)", f.key));
        }
        for ci in chunk0..=chunk_last {
            let a = (ci * CHUNK - fetch_start) as usize;
            let b = (((ci + 1) * CHUNK).min(f.size) - fetch_start) as usize;
            let digest: [u8; 32] = Sha256::digest(&data[a..b]).into();
            if digest != f.leaves[ci as usize] {
                return Err(format!(
                    "object {} changed in S3 since indexing (chunk {ci} mismatch)",
                    f.key
                ));
            }
        }
        let a = (self.pos - fetch_start) as usize;
        let upto = self.end.min(fetch_end);
        let b = (upto - fetch_start) as usize;
        self.pos = upto;
        Ok(Some(data[a..b].to_vec()))
    }
}

/// Streams the DAG under a root as CARv1: header, then blocks in DFS order,
/// deduplicated. dag-pb nodes come from memory; runs of consecutive leaves
/// are fetched from S3 in ranged batches, verified, and emitted.
struct CarBody {
    snap: Rc<Snapshot>,
    s3: Rc<S3Ctx>,
    stack: Vec<Cid>,
    seen: HashSet<[u8; 32]>,
    header: Option<Vec<u8>>,
}

impl CarBody {
    fn new(snap: Rc<Snapshot>, s3: Rc<S3Ctx>, root: Cid) -> CarBody {
        let mut seen = HashSet::new();
        seen.insert(root.digest);
        CarBody {
            snap,
            s3,
            stack: vec![root],
            seen,
            header: Some(ipfs::car_header(&root)),
        }
    }
}

impl Body for CarBody {
    fn pull(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut out = self.header.take().unwrap_or_default();
        while out.len() < CAR_TARGET {
            let Some(cid) = self.stack.pop() else {
                return if out.is_empty() { Ok(None) } else { Ok(Some(out)) };
            };
            if cid.codec == CODEC_DAG_PB {
                let Some(block) = self.snap.nodes.get(&cid.digest) else {
                    return Err(format!("node {cid} missing from the index"));
                };
                ipfs::car_block(&mut out, &cid, block);
                let Some((links, _)) = ipfs::dagpb_decode(block) else {
                    return Err("bad node in index".into());
                };
                for l in links.iter().rev() {
                    if self.seen.insert(l.cid.digest) {
                        self.stack.push(l.cid);
                    }
                }
                continue;
            }
            // A leaf: batch it with consecutive same-file leaves on the
            // stack top into one ranged fetch.
            let Some(&(fi, ci)) = self.snap.leaf_of.get(&cid.digest) else {
                return Err(format!("leaf {cid} missing from the index"));
            };
            let f = &self.snap.files[fi as usize];
            let sizes = ipfs::chunk_sizes(f.size);
            let mut run: Vec<(Cid, u32)> = vec![(cid, ci)];
            let mut span: u64 = sizes[ci as usize];
            while span < STREAM_WINDOW {
                let Some(top) = self.stack.last() else { break };
                if top.codec != CODEC_RAW {
                    break;
                }
                let Some(&(fj, cj)) = self.snap.leaf_of.get(&top.digest) else { break };
                if fj != fi || cj != run.last().unwrap().1 + 1 {
                    break;
                }
                span += sizes[cj as usize];
                run.push((*top, cj));
                self.stack.pop();
            }
            let start = u64::from(run[0].1) * CHUNK;
            let data = if span == 0 {
                Vec::new()
            } else {
                s3::get_range(&self.s3.ep, &self.s3.bucket, &f.key, self.s3.creds.as_ref(), start, span)?
            };
            if data.len() as u64 != span {
                return Err(format!("short read from S3 for {} (object changed?)", f.key));
            }
            let mut off = 0usize;
            for (leaf_cid, cj) in &run {
                let len = sizes[*cj as usize] as usize;
                let bytes = &data[off..off + len];
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                if digest != leaf_cid.digest {
                    return Err(format!(
                        "object {} changed in S3 since indexing (chunk {cj} mismatch)",
                        f.key
                    ));
                }
                ipfs::car_block(&mut out, leaf_cid, bytes);
                off += len;
            }
            // One S3 request per pull: hand back what we have.
            return Ok(Some(out));
        }
        Ok(Some(out))
    }
}
