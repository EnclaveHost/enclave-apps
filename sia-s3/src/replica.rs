//! The S3 key space replicated across independent indexers.
//!
//! An indexer (indexd) is the one party that holds a Sia app's host contracts,
//! funds its host accounts, pins its slabs, and keeps its object list. With one
//! indexer every write depends on that one operator. Here each configured
//! indexer is a full replica (`Store`): the app key may differ per indexer, the
//! data is uploaded to each separately (its own hosts, contracts and funding),
//! and nothing is shared between them but the S3 keys.
//!
//! Consistency, for ONE writer (see "Writers" below):
//!   - Every write is stamped with a version (`Entry::ver`: a millisecond clock
//!     that only moves forward, plus a random write id). Replicas are merged by
//!     taking the newest version of each key; a newer tombstone hides it.
//!   - A write succeeds when `writeQuorum` (W) replicas commit it. Serving
//!     starts once N - W + 1 replicas have loaded, so every write that was
//!     acknowledged is in the merged view (any W set meets any N - W + 1 set).
//!   - Latency: every replica call has a deadline. Once W replicas have
//!     answered, the rest get a short grace period and are then abandoned, so
//!     one slow or hung indexer delays a write by at most that grace (or fails
//!     it at the deadline when W cannot be reached). An abandoned or failed
//!     metadata call has an unknown outcome: the indexer may apply it later.
//!   - Unknown outcomes make a replica *dirty*: its in-memory index may lack
//!     objects that exist, or that will exist at any later time. A dirty
//!     replica still serves what it knows and is re-read from its indexer's
//!     object feed. Nothing assumes such a write has stopped being possible.
//!   - DELETE writes a tombstone into the key's host object on every replica
//!     that holds the key (and, on a replica that does not, a deletion floor
//!     in a ledger object), and answers 204 only once the deletion is durably
//!     recorded (acknowledged by the indexer) on W replicas; otherwise 503,
//!     and a retry re-applies the same deletion. Any W set meets any boot set
//!     of N - W + 1, so a boot always sees the deletion.
//!   - Deletion evidence is never discarded. Maintenance copies every
//!     deletion into a ledger floor on every replica; once a replica has
//!     pinned the floor, the tombstone leaves its object and the payload it
//!     held is reclaimed. A version older than a floor is dead wherever it
//!     shows up: a write abandoned long ago that lands now, or old data an
//!     indexer replays, stays hidden and is purged. Floors cost about 80 bytes
//!     per deleted key, for good.
//!   - A request that fails quorum answers 503; like an S3 500, its write may
//!     still have reached some replicas and may become visible.
//!   - Repair copies the newest version to replicas that lack it (reading it
//!     from any replica that has it) and applies missed deletes.
//!   - Reads go to any replica holding exactly the version being served and
//!     fail over to the next one, resuming at the byte where the last stopped.
//!
//! Fault model: indexers are assumed fail-stop with durable acknowledgements
//! (an acknowledged pin survives). That is what W-of-N and N - W + 1 rely on.
//! Floors held on every replica also cover an indexer that rolls back or
//! loses its state, as long as at most N - W replicas do so at once. An
//! indexer cannot forge objects (they are signed with the app key and checked
//! against Sia sector proofs), but it can withhold or replay them: this is not
//! Byzantine-fault-tolerant storage, and a lying majority defeats it.
//!
//! Writers: there must be one sia-s3 process writing to a set of app keys at a
//! time. indexd offers no fencing (no conditional pin), so a second writer is
//! invisible to this one until a reload, and two writers can make conditional
//! PUTs and version ordering wrong. Requests a stopped writer left in flight
//! are harmless to deletions (floors hide whatever lands later) but may still
//! make an older version of an undeleted key reappear briefly until the newer
//! version wins the merge.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use md5::{Digest, Md5};
use serde::Serialize;
use sia_storage::Object;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::store::{Completion, Cond, Entry, KK, Opts, Placement, Res, Store, Tomb, Ver, md5_hex, new_id, new_wid, now_ms, qkey};

pub struct Replica {
    pub name: String,
    pub indexer: String,
    store: RefCell<Option<Rc<Store>>>,
    pub error: RefCell<Option<String>>,
}

impl Replica {
    pub fn store(&self) -> Option<Rc<Store>> {
        self.store.borrow().clone()
    }
}

/// The merged view: the newest live version of each key (part lists cleared:
/// they differ per replica) and the newest tombstone of each deleted key hash.
#[derive(Default)]
pub struct View {
    pub keys: BTreeMap<KK, Entry>,
    pub tombs: BTreeMap<String, Tomb>,
}

#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterStats {
    pub repair_passes: u64,
    pub repaired_copies: u64,
    pub repaired_deletes: u64,
    pub reconciled: u64,
    pub purged_versions: u64,
    pub floors_written: u64,
    pub reclaimed_tombstones: u64,
    pub refused_writes: u64,
    pub abandoned_calls: u64,
    pub last_repair: Option<String>,
    pub last_repair_error: Option<String>,
}

#[derive(Default, Debug, PartialEq)]
pub struct RepairReport {
    pub reconciled: usize,
    pub purged: usize,
    pub copies: usize,
    pub deletes: usize,
    /// deletion floors written into ledgers
    pub floors: usize,
    /// tombstones (and the payload they held) reclaimed under a floor
    pub reclaimed: usize,
    /// ledgers merged away
    pub merged: usize,
    /// work left for the next pass (byte budget spent, or a replica failed)
    pub pending: usize,
}

/// Timing of replica calls beyond the per-call deadlines in `Opts`.
#[derive(Clone, Debug)]
pub struct Timing {
    /// once a write quorum of uploads has finished, how long the others may still finish
    pub upload_grace: Duration,
    /// once a write quorum of metadata commits has answered, how long the others may still answer
    pub meta_grace: Duration,
    /// how long one replica may stop taking a streamed body before it is dropped from the tee
    pub stall: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { upload_grace: Duration::from_secs(30), meta_grace: Duration::from_secs(5), stall: Duration::from_secs(120) }
    }
}

/// Uploaded bodies awaiting commit, one object per replica that stored it. Nothing in it is pinned as an object, so
/// dropping it leaves only slabs, which `prune_slabs` reclaims.
#[derive(Default)]
pub struct Staged {
    objs: Vec<(usize, Object)>,
}

pub struct Cluster {
    pub replicas: Vec<Replica>,
    pub write_quorum: usize,
    pub opts: Opts,
    pub timing: Timing,
    /// serializes the commit phase of every mutation (uploads run outside it)
    write: Mutex<()>,
    clock: Cell<u64>,
    pub view: RefCell<View>,
    pub stats: RefCell<ClusterStats>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Poll every future to completion on this task (the runtime is single-threaded; calls to different indexers
/// overlap on the network).
pub async fn join_all<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut futs: Vec<Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx: &mut Context<'_>| {
        let mut pending = false;
        for (f, o) in futs.iter_mut().zip(out.iter_mut()) {
            if o.is_none() {
                match f.as_mut().poll(cx) {
                    Poll::Ready(v) => *o = Some(v),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending { Poll::Pending } else { Poll::Ready(()) }
    })
    .await;
    out.into_iter().map(|o| o.expect("polled to completion")).collect()
}

/// Run one call per replica concurrently. Returns when all have finished; or `grace` after `need` of them succeeded;
/// or at `deadline`. Calls still running then are dropped (cancelled) and come back as None.
pub async fn fan_out<T, F: Future<Output = Res<T>>>(
    jobs: Vec<(usize, F)>,
    need: usize,
    grace: Duration,
    deadline: Duration,
    success: impl Fn(&Res<T>) -> bool,
) -> Vec<(usize, Option<Res<T>>)> {
    let ids: Vec<usize> = jobs.iter().map(|(i, _)| *i).collect();
    let mut futs: Vec<Option<Pin<Box<F>>>> = jobs.into_iter().map(|(_, f)| Some(Box::pin(f))).collect();
    let mut out: Vec<Option<Res<T>>> = futs.iter().map(|_| None).collect();
    let mut stop = Box::pin(tokio::time::sleep(deadline));
    let mut grace_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    std::future::poll_fn(|cx: &mut Context<'_>| {
        let (mut running, mut good) = (0, 0);
        for (f, o) in futs.iter_mut().zip(out.iter_mut()) {
            if let Some(fut) = f {
                if let Poll::Ready(r) = fut.as_mut().poll(cx) {
                    *o = Some(r);
                    *f = None;
                }
            }
            match o {
                Some(r) if success(r) => good += 1,
                Some(_) => {}
                None => running += 1,
            }
        }
        if running == 0 || stop.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        if good >= need {
            let g = grace_timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(grace)));
            if g.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    })
    .await;
    drop(futs);
    ids.into_iter().zip(out).collect()
}

/// A writer that counts (and optionally MD5s) what the inner writer accepted, so a failed read can resume at the
/// exact byte and a repair copy can be checked against its ETag.
struct Tap<'a, W> {
    w: &'a mut W,
    n: u64,
    md5: Option<Md5>,
}

impl<'a, W> Tap<'a, W> {
    fn new(w: &'a mut W, md5: bool) -> Self {
        Tap { w, n: 0, md5: md5.then(Md5::new) }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Tap<'_, W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let r = Pin::new(&mut *this.w).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = r {
            this.n += n as u64;
            if let Some(m) = &mut this.md5 {
                m.update(&buf[..n]);
            }
        }
        r
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.w).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.w).poll_shutdown(cx)
    }
}

fn cond_ok(v: &View, kk: &KK, cond: &Cond) -> Res<()> {
    let ok = match cond {
        Cond::None => true,
        Cond::IfNoneMatch => !v.keys.contains_key(kk),
        Cond::IfMatch(t) => v.keys.get(kk).is_some_and(|e| t == "*" || e.t == *t),
    };
    if ok { Ok(()) } else { Err("PreconditionFailed".into()) }
}

fn committed(r: &Res<bool>) -> bool {
    matches!(r, Ok(true))
}

impl Cluster {
    /// `replicas`: (name, indexer URL) in configured order. 1 <= `write_quorum` <= replicas.
    pub fn new(replicas: Vec<(String, String)>, write_quorum: usize, opts: Opts, timing: Timing) -> Cluster {
        assert!(!replicas.is_empty() && (1..=replicas.len()).contains(&write_quorum));
        Cluster {
            replicas: replicas
                .into_iter()
                .map(|(name, indexer)| Replica { name, indexer, store: RefCell::new(None), error: RefCell::new(None) })
                .collect(),
            write_quorum,
            opts,
            timing,
            write: Mutex::new(()),
            clock: Cell::new(0),
            view: Default::default(),
            stats: Default::default(),
        }
    }

    pub fn n(&self) -> usize {
        self.replicas.len()
    }

    /// Replicas that must be loaded before the merged view is complete.
    pub fn read_quorum(&self) -> usize {
        self.n() - self.write_quorum + 1
    }

    pub fn store(&self, i: usize) -> Option<Rc<Store>> {
        self.replicas[i].store()
    }

    pub fn loaded(&self) -> Vec<(usize, Rc<Store>)> {
        self.replicas.iter().enumerate().filter_map(|(i, r)| r.store().map(|s| (i, s))).collect()
    }

    pub fn ready(&self) -> bool {
        self.loaded().len() >= self.read_quorum()
    }

    /// Deadline for one replica's commit-phase call (a few metadata calls in sequence).
    fn meta_deadline(&self) -> Duration {
        self.opts.meta_timeout * 4
    }

    /// Record a replica's last error. It is shown on the unauthenticated status page and /healthz, so callers never
    /// put key names in it (they log those separately).
    fn note(&self, i: usize, e: &str) {
        eprintln!("[sia-s3] replica {}: {e}", self.replicas[i].name);
        *self.replicas[i].error.borrow_mut() = Some(e.to_string());
    }

    fn tick(&self) -> u64 {
        let t = now_ms().max(self.clock.get() + 1);
        self.clock.set(t);
        t
    }

    fn refuse(&self, what: &str, ok: usize, errs: &[String]) -> String {
        self.stats.borrow_mut().refused_writes += 1;
        format!(
            "ServiceUnavailable: {what} reached {ok} of {} replicas, {} required ({})",
            self.n(),
            self.write_quorum,
            if errs.is_empty() { "replicas not loaded".to_string() } else { errs.join("; ") }
        )
    }

    /// Fan a call out to replicas with this cluster's quorum, grace and deadline; abandoned metadata calls mark their
    /// replica ambiguous. Returns the outcomes and the errors (abandoned calls included) for messages.
    async fn fan<T, F: Future<Output = Res<T>>>(
        &self,
        what: &str,
        jobs: Vec<(usize, F)>,
        metadata: bool,
        success: impl Fn(&Res<T>) -> bool,
    ) -> (Vec<(usize, Res<T>)>, Vec<String>) {
        let (grace, deadline) = if metadata {
            (self.timing.meta_grace, self.meta_deadline())
        } else {
            (self.timing.upload_grace, self.opts.upload_timeout + self.opts.meta_timeout * 4)
        };
        let out = fan_out(jobs, self.write_quorum, grace, deadline, success).await;
        let mut done = Vec::new();
        let mut errs = Vec::new();
        for (i, r) in out {
            match r {
                Some(Ok(v)) => done.push((i, Ok(v))),
                Some(Err(e)) => {
                    self.note(i, &format!("{what}: {e}"));
                    errs.push(format!("{}: {e}", self.replicas[i].name));
                    done.push((i, Err(e)));
                }
                None => {
                    self.stats.borrow_mut().abandoned_calls += 1;
                    if metadata {
                        if let Some(s) = self.store(i) {
                            s.mark_ambiguous();
                        }
                    }
                    self.note(i, &format!("{what}: abandoned after the quorum's grace period or the deadline"));
                    errs.push(format!("{}: no answer in time", self.replicas[i].name));
                }
            }
        }
        (done, errs)
    }

    /// Join a replica whose index has loaded, and merge it into the view.
    pub async fn attach(&self, i: usize, store: Rc<Store>) {
        let _w = self.write.lock().await;
        *self.replicas[i].store.borrow_mut() = Some(store);
        *self.replicas[i].error.borrow_mut() = None;
        self.rebuild().await;
    }

    /// Re-read a replica's index from its indexer and swap it in, unless the replica changed meanwhile. The
    /// replica's ambiguity time carries over: a listing that began before the last unknown outcome leaves it dirty.
    pub async fn reconcile(&self, i: usize) -> Res<()> {
        let old = self.store(i).ok_or("not loaded")?;
        let before = old.changes.get();
        let fresh = Store::load_with(old.sdk.clone(), old.compact_max_object, old.opts.clone()).await?;
        let _w = self.write.lock().await;
        let same = self.store(i).is_some_and(|s| Rc::ptr_eq(&s, &old));
        if !same || old.changes.get() != before {
            return Err("the replica changed while it was being re-read".into());
        }
        fresh.ambiguous_ms.set(old.ambiguous_ms.get());
        *fresh.stats.borrow_mut() = old.stats.borrow().clone();
        {
            // uploads begun here but with no part yet exist only in memory
            let mut fi = fresh.idx.lock().await;
            for (id, u) in old.idx.lock().await.uploads.iter() {
                fi.uploads.entry(id.clone()).or_insert_with(|| crate::store::Upload {
                    b: u.b.clone(),
                    k: u.k.clone(),
                    started: u.started,
                    parts: BTreeMap::new(),
                });
            }
        }
        *self.replicas[i].store.borrow_mut() = Some(Rc::new(fresh));
        self.rebuild().await;
        self.stats.borrow_mut().reconciled += 1;
        Ok(())
    }

    /// Recompute the whole merged view from the loaded replicas.
    pub async fn rebuild(&self) {
        let mut v = View::default();
        let mut clock = self.clock.get();
        for (_, s) in self.loaded() {
            let idx = s.idx.lock().await;
            for (kk, k) in &idx.keys {
                clock = clock.max(k.entry.ver().h);
                if v.keys.get(kk).is_none_or(|e| k.entry.ver() > e.ver()) {
                    let mut e = k.entry.clone();
                    e.x.clear();
                    v.keys.insert(kk.clone(), e);
                }
            }
            let held = idx.tombs.iter().flat_map(|(q, ts)| ts.iter().map(move |(_, t)| (q, t)));
            for (q, t) in held.chain(idx.floors.iter()) {
                clock = clock.max(t.h);
                if v.tombs.get(q).is_none_or(|c| t.ver() > c.ver()) {
                    v.tombs.insert(q.clone(), t.clone());
                }
            }
            for o in idx.oldest.values() {
                clock = clock.max(o.h);
            }
        }
        let tombs = &v.tombs;
        v.keys.retain(|kk, e| tombs.get(&qkey(&kk.0, &kk.1)).is_none_or(|t| t.ver() < e.ver()));
        self.clock.set(clock);
        *self.view.borrow_mut() = v;
    }

    /// Recompute one key of the merged view.
    async fn refresh_key(&self, kk: &KK) {
        let q = qkey(&kk.0, &kk.1);
        let (mut live, mut tomb): (Option<Entry>, Option<Tomb>) = (None, None);
        for (_, s) in self.loaded() {
            let idx = s.idx.lock().await;
            if let Some(k) = idx.keys.get(kk) {
                if live.as_ref().is_none_or(|e| k.entry.ver() > e.ver()) {
                    live = Some(k.entry.clone());
                }
            }
            if let Some(t) = idx.tomb(&q) {
                if tomb.as_ref().is_none_or(|c| t.ver() > c.ver()) {
                    tomb = Some(t.clone());
                }
            }
        }
        let mut v = self.view.borrow_mut();
        match live.filter(|e| tomb.as_ref().is_none_or(|t| t.ver() < e.ver())) {
            Some(mut e) => {
                e.x.clear();
                v.keys.insert(kk.clone(), e);
            }
            None => {
                v.keys.remove(kk);
            }
        }
        match tomb {
            Some(t) => v.tombs.insert(q, t),
            None => v.tombs.remove(&q),
        };
    }

    pub fn lookup(&self, b: &str, k: &str) -> Option<Entry> {
        self.view.borrow().keys.get(&(b.to_string(), k.to_string())).cloned()
    }

    /// Can some loaded replica serve `[start, start+len)` of version `e` from its index? Checked before a GET sends
    /// its head, so a version no replica can serve answers 500 rather than a short body.
    pub async fn servable(&self, e: &Entry, start: u64, len: u64) -> Res<()> {
        let kk = (e.b.clone(), e.k.clone());
        let mut last = "no loaded replica holds this version".to_string();
        for (_, s) in self.loaded() {
            let idx = s.idx.lock().await;
            if let Some(k) = idx.keys.get(&kk).filter(|k| k.entry.ver() == e.ver()) {
                match idx.segments(k, start, len) {
                    Ok(_) => return Ok(()),
                    Err(err) => last = err,
                }
            }
        }
        Err(last)
    }

    /// Stream `[start, start+len)` of version `e` into `w` from any replica holding exactly that version, failing
    /// over to the next one and resuming at the first byte not yet written. Each segment read has a deadline of the
    /// stall time plus one second per MiB, so a hung replica costs at most that.
    pub async fn read<W: AsyncWrite + Unpin>(&self, e: &Entry, start: u64, len: u64, w: &mut W) -> Res<u64> {
        if len == 0 {
            return Ok(0);
        }
        let kk = (e.b.clone(), e.k.clone());
        let ver = e.ver();
        let mut sent = 0u64;
        let mut errs = Vec::new();
        for (i, s) in self.loaded() {
            let segs = {
                let idx = s.idx.lock().await;
                match idx.keys.get(&kk) {
                    Some(k) if k.entry.ver() == ver => idx.segments(k, start + sent, len - sent),
                    _ => continue,
                }
            };
            let segs = match segs {
                Ok(s) => s,
                Err(e) => {
                    errs.push(format!("{}: {e}", self.replicas[i].name));
                    continue;
                }
            };
            let mut tap = Tap::new(w, false);
            let mut failed = None;
            for (obj, off, n) in segs {
                let deadline = self.timing.stall + Duration::from_secs(n >> 20);
                match tokio::time::timeout(deadline, s.read_into(&obj, off, n, &mut tap)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        failed = Some(e);
                        break;
                    }
                    Err(_) => {
                        failed = Some(format!("no progress within {deadline:?}"));
                        break;
                    }
                }
            }
            sent += tap.n;
            match failed {
                None if sent == len => return Ok(sent),
                None => return Err(format!("short read {sent}/{len}")),
                Some(e) => {
                    eprintln!("[sia-s3] GET {}/{}: replica {} failed after {sent} bytes", kk.0, kk.1, self.replicas[i].name);
                    self.note(i, &format!("read: {e}"));
                    errs.push(format!("{}: {e}", self.replicas[i].name));
                }
            }
        }
        Err(if errs.is_empty() { "no loaded replica holds this version".into() } else { errs.join("; ") })
    }

    pub fn check(&self, b: &str, k: &str, cond: &Cond) -> Res<()> {
        cond_ok(&self.view.borrow(), &(b.to_string(), k.to_string()), cond)
    }

    fn staged(&self, what: &str, results: Vec<(usize, Res<Object>)>, errs: Vec<String>) -> Res<Staged> {
        let objs: Vec<(usize, Object)> = results.into_iter().filter_map(|(i, r)| r.ok().map(|o| (i, o))).collect();
        if objs.len() < self.write_quorum {
            return Err(self.refuse(what, objs.len(), &errs));
        }
        Ok(Staged { objs })
    }

    /// Upload a buffered body to every loaded replica.
    pub async fn upload(&self, body: Vec<u8>) -> Res<Staged> {
        let stores = self.loaded();
        if stores.len() < self.write_quorum {
            return Err(self.refuse("upload", 0, &[]));
        }
        let body: Rc<[u8]> = body.into();
        let jobs = stores
            .into_iter()
            .map(|(i, s)| {
                let body = body.clone();
                (i, async move { s.upload(body).await })
            })
            .collect();
        let (results, errs) = self.fan("upload", jobs, false, Result::is_ok).await;
        self.staged("upload", results, errs)
    }

    /// Upload a body streamed from `reader` to every loaded replica at once. A replica whose upload fails, or that
    /// stops taking data for the stall time, is dropped from the tee; the others continue. A reader that ends early
    /// ends every upload short; the caller's size and hash checks refuse those.
    pub async fn upload_reader<R: AsyncRead + Unpin>(&self, mut reader: R) -> Res<Staged> {
        let stores = self.loaded();
        if stores.len() < self.write_quorum {
            return Err(self.refuse("upload", 0, &[]));
        }
        let mut txs = Vec::new();
        let mut jobs = Vec::new();
        for (i, s) in stores {
            let (tx, rx) = tokio::io::duplex(1 << 20);
            txs.push(Some(tx));
            jobs.push((i, async move { s.upload_reader(rx).await }));
        }
        let stall = self.timing.stall;
        let pump = async {
            let mut buf = vec![0u8; 256 << 10];
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let mut live = false;
                for slot in txs.iter_mut() {
                    if let Some(tx) = slot {
                        if matches!(tokio::time::timeout(stall, tx.write_all(&buf[..n])).await, Ok(Ok(()))) {
                            live = true;
                        } else {
                            *slot = None;
                        }
                    }
                }
                if !live {
                    break;
                }
            }
            for tx in txs.iter_mut().flatten() {
                let _ = tokio::time::timeout(stall, tx.shutdown()).await;
            }
            txs.clear();
        };
        let ((), (results, errs)) = tokio::join!(pump, self.fan("upload", jobs, false, Result::is_ok));
        self.staged("upload", results, errs)
    }

    /// Drop staged uploads that will not be committed. Nothing was pinned as an object, so there is nothing to
    /// delete: `prune_slabs` reclaims the slabs.
    pub async fn discard(&self, staged: Staged) {
        drop(staged);
    }

    pub async fn put(&self, b: &str, k: &str, body: Vec<u8>, ct: Option<String>, cond: &Cond) -> Res<String> {
        self.check(b, k, cond)?;
        let etag = md5_hex(&body);
        let size = body.len() as u64;
        let staged = self.upload(body).await?;
        self.commit(b, k, staged, etag.clone(), size, ct, cond).await?;
        Ok(etag)
    }

    /// Make staged uploads the key's content as one new version: placements (manifests, if the entry does not fit
    /// inline) are uploaded first, outside the write lock; then, under it, the condition is checked again and the
    /// version committed.
    #[allow(clippy::too_many_arguments)]
    pub async fn commit(&self, b: &str, k: &str, staged: Staged, etag: String, size: u64, ct: Option<String>, cond: &Cond) -> Res<()> {
        let proto = Entry { b: b.into(), k: k.into(), s: size, t: etag, m: 0, c: ct, x: vec![], h: 0, i: String::new() };
        let jobs = staged
            .objs
            .into_iter()
            .filter_map(|(i, o)| {
                let (s, proto) = (self.store(i)?, proto.clone());
                Some((i, async move { s.stage_put(&o, &proto).await.map(|p| (o, p)) }))
            })
            .collect();
        let (placed, errs) = self.fan("place", jobs, false, Result::is_ok).await;
        let placed: Vec<(usize, (Object, Placement))> = placed.into_iter().filter_map(|(i, r)| r.ok().map(|p| (i, p))).collect();
        if placed.len() < self.write_quorum {
            return Err(self.refuse("PUT", placed.len(), &errs));
        }

        let _w = self.write.lock().await;
        let kk = (b.to_string(), k.to_string());
        self.check(b, k, cond)?;
        let entry = Entry { m: now(), h: self.tick(), i: new_wid(), ..proto };
        let jobs = placed
            .into_iter()
            .filter_map(|(i, (o, p))| {
                let (s, e) = (self.store(i)?, entry.clone());
                Some((i, async move { s.commit_put(o, p, e, false).await }))
            })
            .collect();
        let (results, errs) = self.fan("PUT", jobs, true, committed).await;
        let ok = results.iter().filter(|(_, r)| committed(r)).count();
        self.refresh_key(&kk).await;
        if ok >= self.write_quorum { Ok(()) } else { Err(self.refuse("PUT", ok, &errs)) }
    }

    pub async fn copy(&self, src: (&str, &str), dst: (&str, &str)) -> Res<(String, i64)> {
        let skk = (src.0.to_string(), src.1.to_string());
        let dkk = (dst.0.to_string(), dst.1.to_string());
        let se = self.view.borrow().keys.get(&skk).cloned();
        let Some(se) = se else { return Err("NoSuchKey".into()) };
        if skk == dkk {
            return Ok((se.t, se.m));
        }
        let sv = se.ver();
        let proto = Entry { b: dkk.0.clone(), k: dkk.1.clone(), m: 0, h: 0, i: String::new(), x: vec![], ..se };
        let jobs = self
            .loaded()
            .into_iter()
            .map(|(i, s)| {
                let (skk, sv, proto) = (skk.clone(), sv.clone(), proto.clone());
                (i, async move { s.stage_copy(&skk, &sv, &proto).await })
            })
            .collect();
        let (placed, errs) = self.fan("copy", jobs, false, |r| matches!(r, Ok(Some(_)))).await;
        let placed: Vec<(usize, Placement)> = placed.into_iter().filter_map(|(i, r)| r.ok().flatten().map(|p| (i, p))).collect();
        if placed.len() < self.write_quorum {
            return Err(self.refuse("copy", placed.len(), &errs));
        }

        let _w = self.write.lock().await;
        if self.view.borrow().keys.get(&skk).map(|e| e.ver()) != Some(sv.clone()) {
            return Err("ServiceUnavailable: the copy source changed while the copy was prepared; retry".into());
        }
        let d = Entry { m: now(), h: self.tick(), i: new_wid(), ..proto };
        let jobs = placed
            .into_iter()
            .filter_map(|(i, p)| {
                let (s, skk, sv, d) = (self.store(i)?, skk.clone(), sv.clone(), d.clone());
                Some((i, async move { s.commit_copy(&skk, &sv, d, p).await }))
            })
            .collect();
        let (results, errs) = self.fan("copy", jobs, true, committed).await;
        let ok = results.iter().filter(|(_, r)| committed(r)).count();
        self.refresh_key(&dkk).await;
        if ok >= self.write_quorum { Ok((d.t, d.m)) } else { Err(self.refuse("copy", ok, &errs)) }
    }

    pub async fn delete(&self, b: &str, k: &str) -> Res<()> {
        let kk = (b.to_string(), k.to_string());
        let q = qkey(b, k);
        let (t, errs) = {
            let _w = self.write.lock().await;
            let (live, tomb) = {
                let v = self.view.borrow();
                (v.keys.contains_key(&kk), v.tombs.get(&q).cloned())
            };
            let t = match (live, tomb) {
                (true, _) => Tomb { q: q.clone(), h: self.tick(), i: new_wid() },
                // deleted already, perhaps not yet durably (a retried 503): record it wherever it is still missing
                (false, Some(t)) => t,
                (false, None) => return Ok(()),
            };
            let jobs = self
                .loaded()
                .into_iter()
                .map(|(i, s)| {
                    let (kk, t) = (kk.clone(), t.clone());
                    (i, async move { s.tombstone(&kk, &t).await })
                })
                .collect();
            let (_, errs) = self.fan("DELETE", jobs, true, committed).await;
            self.refresh_key(&kk).await;
            (t, errs)
        };
        let (mut recorded, mut missing) = self.recorded(&t).await;
        let mut errs = errs;
        if recorded < self.write_quorum && !missing.is_empty() {
            // the others record the deletion as a floor in a new ledger (an upload, so outside the write lock:
            // floors only ever add, and a floor hides the key wherever it is)
            let jobs = missing
                .drain(..)
                .map(|(i, s)| {
                    let t = t.clone();
                    (i, async move { s.write_ledger(vec![t]).await })
                })
                .collect();
            let (_, more) = self.fan("DELETE floor", jobs, false, Result::is_ok).await;
            errs.extend(more);
            recorded = self.recorded(&t).await.0;
        }
        if recorded >= self.write_quorum {
            Ok(())
        } else {
            self.stats.borrow_mut().refused_writes += 1;
            Err(format!(
                "ServiceUnavailable: DELETE recorded durably on {recorded} of {} replicas, {} required{}",
                self.n(),
                self.write_quorum,
                if errs.is_empty() { String::new() } else { format!(" ({})", errs.join("; ")) }
            ))
        }
    }

    /// Replicas that durably hold deletion `t` (a tombstone or floor at least as new), and the loaded ones that do
    /// not (a floor in a new ledger records it there, whether or not they hold the key).
    async fn recorded(&self, t: &Tomb) -> (usize, Vec<(usize, Rc<Store>)>) {
        let mut recorded = 0;
        let mut missing = Vec::new();
        for (i, s) in self.loaded() {
            if s.idx.lock().await.tomb(&t.q).is_some_and(|x| x.ver() >= t.ver()) {
                recorded += 1;
            } else {
                missing.push((i, s.clone()));
            }
        }
        (recorded, missing)
    }

    async fn find_upload(&self, uid: &str) -> Option<(String, String, i64)> {
        for (_, s) in self.loaded() {
            if let Some(u) = s.idx.lock().await.uploads.get(uid) {
                return Some((u.b.clone(), u.k.clone(), u.started));
            }
        }
        None
    }

    pub async fn create_upload(&self, b: &str, k: &str) -> Res<String> {
        let stores = self.loaded();
        if stores.len() < self.write_quorum {
            return Err(self.refuse("CreateMultipartUpload", stores.len(), &[]));
        }
        let uid = new_id();
        let started = now();
        for (_, s) in stores {
            s.ensure_upload(&uid, b, k, started).await;
        }
        Ok(uid)
    }

    /// (upload id, bucket, key, started) of every open upload any loaded replica knows.
    pub async fn uploads(&self) -> Vec<(String, String, String, i64)> {
        let mut out: BTreeMap<String, (String, String, i64)> = BTreeMap::new();
        for (_, s) in self.loaded() {
            for (id, u) in &s.idx.lock().await.uploads {
                out.entry(id.clone()).or_insert((u.b.clone(), u.k.clone(), u.started));
            }
        }
        out.into_iter().map(|(id, (b, k, t))| (id, b, k, t)).collect()
    }

    pub async fn upload_part(&self, uid: &str, n: u32, body: Vec<u8>) -> Res<String> {
        let (b, k, started) = self.find_upload(uid).await.ok_or("NoSuchUpload")?;
        let stores = self.loaded();
        if stores.len() < self.write_quorum {
            return Err(self.refuse("UploadPart", 0, &[]));
        }
        let body: Rc<[u8]> = body.into();
        let etag = md5_hex(&body);
        let jobs = stores
            .into_iter()
            .map(|(i, s)| {
                let (body, b, k) = (body.clone(), b.clone(), k.clone());
                (i, async move {
                    s.ensure_upload(uid, &b, &k, started).await;
                    s.upload_part(uid, n, body).await
                })
            })
            .collect();
        let (results, errs) = self.fan("UploadPart", jobs, false, Result::is_ok).await;
        if !results.is_empty() && results.iter().all(|(_, r)| matches!(r, Err(e) if e == "NoSuchUpload")) {
            return Err("NoSuchUpload".into()); // aborted meanwhile
        }
        let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
        if ok >= self.write_quorum { Ok(etag) } else { Err(self.refuse("UploadPart", ok, &errs)) }
    }

    pub async fn complete_upload(&self, uid: &str, want: &[(u32, String)]) -> Res<(String, String, String)> {
        let (b, k, _) = self.find_upload(uid).await.ok_or("NoSuchUpload")?;
        let mut holders = Vec::new();
        for (i, s) in self.loaded() {
            if s.idx.lock().await.uploads.contains_key(uid) {
                holders.push((i, s));
            }
        }
        let jobs = holders.iter().map(|(i, s)| (*i, async move { s.stage_complete(uid, want).await })).collect();
        let (staged, errs) = self.fan("CompleteMultipartUpload", jobs, false, Result::is_ok).await;
        let client = staged
            .iter()
            .find_map(|(_, r)| r.as_ref().err().filter(|e| e.starts_with("InvalidPart") || e.starts_with("NoSuchUpload")).cloned());
        let staged: Vec<(usize, Completion)> = staged.into_iter().filter_map(|(i, r)| r.ok().map(|c| (i, c))).collect();
        if staged.len() < self.write_quorum {
            return Err(match (staged.len(), client) {
                (0, Some(e)) => e, // the request itself is wrong
                (n, _) => self.refuse("CompleteMultipartUpload", n, &errs),
            });
        }

        let _w = self.write.lock().await;
        let (m, h, wid) = (now(), self.tick(), new_wid());
        let jobs = staged
            .into_iter()
            .filter_map(|(i, c)| {
                let (s, wid) = (self.store(i)?, wid.clone());
                Some((i, async move { s.commit_complete(uid, c, m, h, &wid).await }))
            })
            .collect();
        let (results, errs) = self.fan("CompleteMultipartUpload", jobs, true, Result::is_ok).await;
        let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
        if ok >= self.write_quorum {
            // a replica that did not complete keeps no half upload: repair copies the completed key to it
            for (i, s) in &holders {
                if !results.iter().any(|(j, r)| j == i && r.is_ok()) {
                    let _ = tokio::time::timeout(self.meta_deadline(), s.abort_upload(uid)).await;
                }
            }
        }
        self.refresh_key(&(b.clone(), k.clone())).await;
        let etag = results.into_iter().find_map(|(_, r)| r.ok()).map(|r| r.2);
        match etag {
            Some(etag) if ok >= self.write_quorum => Ok((b, k, etag)),
            _ => Err(self.refuse("CompleteMultipartUpload", ok, &errs)),
        }
    }

    pub async fn abort_upload(&self, uid: &str) -> Res<()> {
        let jobs = self.loaded().into_iter().map(|(i, s)| (i, async move { s.abort_upload(uid).await })).collect();
        let out = fan_out(jobs, usize::MAX, Duration::ZERO, self.meta_deadline(), Result::is_ok).await;
        let mut found = false;
        let mut errs = Vec::new();
        for (i, r) in out {
            match r {
                Some(Ok(())) => found = true,
                Some(Err(e)) if e == "NoSuchUpload" => {}
                Some(Err(e)) => {
                    found = true;
                    self.note(i, &format!("AbortMultipartUpload: {e}"));
                    errs.push(format!("{}: {e}", self.replicas[i].name));
                }
                None => {
                    found = true;
                    errs.push(format!("{}: no answer in time", self.replicas[i].name));
                }
            }
        }
        match (found, errs.is_empty()) {
            (false, _) => Err("NoSuchUpload".into()),
            (true, true) => Ok(()),
            (true, false) => Err(format!("ServiceUnavailable: AbortMultipartUpload: {}", errs.join("; "))),
        }
    }

    /// Copy version `e` (from whichever replica serves it) into a new upload on `target`, checked against its size
    /// and, for single-part ETags, its MD5.
    async fn copy_version(&self, e: &Entry, target: &Store) -> Res<Object> {
        if e.s == 0 {
            return target.upload(Rc::from(&[][..])).await;
        }
        let (mut tx, rx) = tokio::io::duplex(4 << 20);
        let pump = async {
            let mut tap = Tap::new(&mut tx, true);
            let r = self.read(e, 0, e.s, &mut tap).await;
            let md5 = tap.md5.take().map(|m| hex::encode(m.finalize()));
            drop(tap);
            let _ = tx.shutdown().await;
            drop(tx);
            (r, md5)
        };
        let ((read, md5), up) = tokio::join!(pump, target.upload_reader(rx));
        let obj = up?;
        let bad = match read {
            Ok(n) if n == e.s => (!e.t.contains('-') && md5.as_deref() != Some(e.t.as_str())).then(|| "MD5 differs from the ETag".to_string()),
            Ok(n) => Some(format!("read {n} of {} bytes", e.s)),
            Err(err) => Some(err),
        };
        match bad {
            Some(err) => Err(err),
            None => Ok(obj),
        }
    }

    /// One maintenance pass: re-read ambiguous replicas, purge dead versions, apply missed deletes, copy up to
    /// `max_bytes` of newest versions to replicas that lack them, then collect tombstones behind the barrier.
    pub async fn repair(&self, max_bytes: u64) -> RepairReport {
        let mut rep = RepairReport::default();
        for (i, s) in self.loaded() {
            // dirty: the index may lack objects an unknown outcome created
            if s.dirty() {
                match self.reconcile(i).await {
                    Ok(()) => rep.reconciled += 1,
                    Err(e) => {
                        self.note(i, &format!("reconcile: {e}"));
                        rep.pending += 1;
                    }
                }
            }
        }
        for (i, s) in self.loaded() {
            match s.purge_dead(64).await {
                Ok(n) => rep.purged += n,
                Err(e) => {
                    self.note(i, &format!("purge: {e}"));
                    rep.pending += 1;
                }
            }
        }
        if rep.purged > 0 {
            self.rebuild().await;
        }

        if self.n() > 1 {
            self.repair_replicas(max_bytes, &mut rep).await;
        }
        self.floors(&mut rep).await;
        let mut st = self.stats.borrow_mut();
        st.repair_passes += 1;
        st.repaired_copies += rep.copies as u64;
        st.repaired_deletes += rep.deletes as u64;
        st.purged_versions += rep.purged as u64;
        st.floors_written += rep.floors as u64;
        st.reclaimed_tombstones += rep.reclaimed as u64;
        st.last_repair = Some(chrono::Utc::now().to_rfc3339());
        st.last_repair_error = (rep.pending > 0).then(|| format!("{} items left for the next pass", rep.pending));
        rep
    }

    async fn repair_replicas(&self, max_bytes: u64, rep: &mut RepairReport) {
        let (keys, tombs) = {
            let v = self.view.borrow();
            (v.keys.clone(), v.tombs.clone())
        };
        let mut copies: Vec<(usize, Entry)> = Vec::new();
        let mut deletes: Vec<(usize, KK, Tomb)> = Vec::new();
        for (i, s) in self.loaded() {
            let idx = s.idx.lock().await;
            for (kk, e) in &keys {
                if idx.keys.get(kk).is_none_or(|k| k.entry.ver() < e.ver()) {
                    copies.push((i, e.clone()));
                }
            }
            for (kk, k) in &idx.keys {
                if let Some(t) = tombs.get(&qkey(&kk.0, &kk.1)).filter(|t| k.entry.ver() < t.ver()) {
                    deletes.push((i, kk.clone(), t.clone()));
                }
            }
        }

        for (i, kk, t) in deletes {
            let _w = self.write.lock().await;
            let Some(s) = self.store(i) else { continue };
            match tokio::time::timeout(self.meta_deadline(), s.tombstone(&kk, &t)).await {
                Ok(Ok(_)) => rep.deletes += 1,
                Ok(Err(e)) => {
                    self.note(i, &format!("repair delete: {e}"));
                    rep.pending += 1;
                }
                Err(_) => {
                    s.mark_ambiguous();
                    self.note(i, "repair delete: no answer in time");
                    rep.pending += 1;
                }
            }
            self.refresh_key(&kk).await;
        }

        let mut spent = 0u64;
        for (i, e) in copies {
            if spent > 0 && spent + e.s > max_bytes {
                rep.pending += 1;
                continue;
            }
            spent += e.s;
            let Some(target) = self.store(i) else { continue };
            let staged = async {
                let obj = self.copy_version(&e, &target).await?;
                let p = target.stage_put(&obj, &e).await?;
                Ok::<_, String>((obj, p))
            };
            let (obj, placement) = match tokio::time::timeout(self.opts.upload_timeout * 2, staged).await {
                Ok(Ok(v)) => v,
                Ok(Err(err)) => {
                    eprintln!("[sia-s3] repair: copy of {}/{} to {} failed", e.b, e.k, self.replicas[i].name);
                    self.note(i, &format!("repair copy: {err}"));
                    rep.pending += 1;
                    continue;
                }
                Err(_) => {
                    self.note(i, "repair copy: no completion in time");
                    rep.pending += 1;
                    continue;
                }
            };
            let _w = self.write.lock().await;
            let kk = (e.b.clone(), e.k.clone());
            // the key may have been written or deleted while the copy ran
            let current = self.view.borrow().keys.get(&kk).is_some_and(|v| v.ver() == e.ver());
            if !current {
                continue;
            }
            match tokio::time::timeout(self.meta_deadline(), target.commit_put(obj, placement, e.clone(), false)).await {
                Ok(Ok(_)) => rep.copies += 1,
                Ok(Err(err)) => {
                    self.note(i, &format!("repair commit: {err}"));
                    rep.pending += 1;
                }
                Err(_) => {
                    target.mark_ambiguous();
                    self.note(i, "repair commit: no answer in time");
                    rep.pending += 1;
                }
            }
            self.refresh_key(&kk).await;
        }
    }

    /// Copy every deletion the view knows into a ledger floor on every loaded replica that lacks it, then reclaim
    /// the tombstones (and payload) floors cover, and merge ledgers that have piled up. Never drops a floor.
    async fn floors(&self, rep: &mut RepairReport) {
        let deletions: Vec<Tomb> = self.view.borrow().tombs.values().cloned().collect();
        for (i, s) in self.loaded() {
            let missing: Vec<Tomb> = {
                let idx = s.idx.lock().await;
                deletions.iter().filter(|t| idx.floors.get(&t.q).is_none_or(|f| f.ver() < t.ver())).cloned().collect()
            };
            if !missing.is_empty() {
                let n = missing.len();
                match tokio::time::timeout(self.opts.upload_timeout + self.meta_deadline(), s.write_ledger(missing)).await {
                    Ok(Ok(())) => rep.floors += n,
                    Ok(Err(e)) => {
                        self.note(i, &format!("ledger: {e}"));
                        rep.pending += 1;
                        continue;
                    }
                    Err(_) => {
                        s.mark_ambiguous();
                        self.note(i, "ledger: no completion in time");
                        rep.pending += 1;
                        continue;
                    }
                }
            }
            match tokio::time::timeout(self.meta_deadline() * 8, s.reclaim_covered()).await {
                Ok(Ok(n)) => rep.reclaimed += n,
                Ok(Err(e)) => {
                    self.note(i, &format!("reclaim: {e}"));
                    rep.pending += 1;
                }
                Err(_) => {
                    s.mark_ambiguous();
                    rep.pending += 1;
                }
            }
            match tokio::time::timeout(self.opts.upload_timeout + self.meta_deadline() * 8, s.merge_ledgers()).await {
                Ok(Ok(n)) => rep.merged += n,
                Ok(Err(e)) => {
                    self.note(i, &format!("ledger merge: {e}"));
                    rep.pending += 1;
                }
                Err(_) => {
                    s.mark_ambiguous();
                    rep.pending += 1;
                }
            }
        }
        self.rebuild().await;
    }

    /// Per-replica state for /healthz and the status page (no keys or object names).
    pub async fn replica_status(&self) -> Vec<serde_json::Value> {
        let view_keys: Vec<(KK, Ver)> = self.view.borrow().keys.iter().map(|(kk, e)| (kk.clone(), e.ver())).collect();
        let mut out = Vec::new();
        for r in &self.replicas {
            let mut v = serde_json::json!({ "name": r.name, "indexer": r.indexer, "loaded": false, "error": *r.error.borrow() });
            if let Some(s) = r.store() {
                let idx = s.idx.lock().await;
                let behind = view_keys.iter().filter(|(kk, ver)| idx.keys.get(kk).is_none_or(|k| k.entry.ver() != *ver)).count();
                v["loaded"] = true.into();
                v["keys"] = idx.keys.len().into();
                v["objects"] = idx.objs.len().into();
                v["behind"] = behind.into();
                v["tombstonedKeys"] = idx.tombs.len().into();
                v["manifests"] = idx.manifests.len().into();
                v["unreadManifests"] = idx.unresolved.len().into();
                v["floors"] = idx.floors.len().into();
                v["ledgers"] = idx.ledgers.len().into();
                v["unreadLedgers"] = idx.unread_ledgers.len().into();
                v["openUploads"] = idx.uploads.len().into();
                v["dirty"] = s.dirty().into();
                v["compactor"] = serde_json::json!(*s.stats.borrow());
            }
            out.push(v);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sia_storage::AppKey;
    use sia_storage::mock::{Fault, MockNetwork};

    /// One independent indexer: its own mock indexer, hosts and app key.
    struct Net {
        net: MockNetwork,
        key: AppKey,
    }

    fn opts() -> Opts {
        Opts { shards: (3, 9), meta_timeout: Duration::from_millis(1500), upload_timeout: Duration::from_secs(60) }
    }

    /// Grace periods long enough that a healthy replica is never abandoned under test load (the fault tests
    /// shorten them).
    fn timing() -> Timing {
        Timing { upload_grace: Duration::from_secs(30), meta_grace: Duration::from_secs(5), stall: Duration::from_secs(5) }
    }

    fn short() -> Timing {
        Timing { upload_grace: Duration::from_millis(300), meta_grace: Duration::from_millis(300), ..timing() }
    }

    /// Where disk-backed mock networks make their sector directories: under this crate's own `target/`, which is
    /// on disk (not a tmpfs `/tmp`). Each network makes a fresh, uniquely named directory here and removes it, and
    /// only the files it wrote, when it is dropped.
    fn sector_base() -> std::path::PathBuf {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("mock-sectors");
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    impl Net {
        fn new(seed: u8) -> Net {
            let net = MockNetwork::new();
            net.add_hosts(12); // 3 + 9 shards: the smallest layout the SDK accepts
            Net { net, key: AppKey::import([seed; 32]) }
        }
        /// As `new`, but the mock hosts keep their (real, encrypted, erasure-coded) sectors on disk instead of in
        /// RAM: for the tests that write enough 4 MiB sectors to exhaust memory. Same SDK, same coverage.
        fn on_disk(seed: u8) -> Net {
            Net::on_disk_with(seed, 64 << 30)
        }
        fn on_disk_with(seed: u8, limit: u64) -> Net {
            let net = MockNetwork::with_disk_sectors(&sector_base(), limit).unwrap();
            net.add_hosts(12);
            Net { net, key: AppKey::import([seed; 32]) }
        }
        async fn load(&self) -> Res<Rc<Store>> {
            let sdk = self.net.sdk(self.key.clone()).await.map_err(|e| e.to_string())?;
            Ok(Rc::new(Store::load_with(sdk, 8 << 20, opts()).await?))
        }
        fn offline(&self, off: bool) {
            self.net.set_indexer_offline(off);
        }
    }

    async fn cluster(nets: &[Net], w: usize) -> Cluster {
        cluster_with(nets, w, timing()).await
    }

    async fn cluster_with(nets: &[Net], w: usize, timing: Timing) -> Cluster {
        let c = Cluster::new(
            (0..nets.len()).map(|i| (format!("r{i}"), format!("https://indexer-{i}.example"))).collect(),
            w,
            opts(),
            timing,
        );
        for (i, n) in nets.iter().enumerate() {
            if let Ok(s) = n.load().await {
                c.attach(i, s).await;
            }
        }
        c
    }

    async fn get(c: &Cluster, k: &str) -> Option<Vec<u8>> {
        let e = c.lookup("b", k)?;
        let mut out = Vec::new();
        c.read(&e, 0, e.s, &mut out).await.unwrap();
        Some(out)
    }

    /// On one indexer, listed fresh from its object feed (not any in-memory index): the key's live version, whether
    /// deletion evidence (tombstone or floor) names it, and whether any object still names any version of it.
    async fn on_disk(n: &Net, k: &str) -> (Option<Ver>, bool, bool) {
        let s = n.load().await.unwrap();
        let idx = s.idx.lock().await;
        let q = qkey("b", k);
        (idx.keys.get(&("b".to_string(), k.to_string())).map(|k| k.entry.ver()), idx.tomb(&q).is_some(), idx.oldest.contains_key(&q))
    }

    /// Run maintenance passes until one finds nothing to do.
    async fn maintain(c: &Cluster) -> RepairReport {
        let mut total = RepairReport::default();
        for _ in 0..6 {
            let r = c.repair(1 << 30).await;
            let idle = r.reconciled + r.purged + r.copies + r.deletes + r.floors + r.reclaimed + r.merged + r.pending == 0;
            total.reconciled += r.reconciled;
            total.purged += r.purged;
            total.copies += r.copies;
            total.deletes += r.deletes;
            total.floors += r.floors;
            total.reclaimed += r.reclaimed;
            total.merged += r.merged;
            total.pending = r.pending;
            if idle {
                break;
            }
        }
        total
    }

    #[tokio::test]
    async fn a_lost_commit_response_cannot_resurrect_a_deleted_key() {
        // r1 pins v2 but the answer is lost, so r1's index still says v1 while the indexer holds v2. The 0.2
        // draft's time-based GC dropped every tombstone and a reload brought v2 back (reproduced before the fix).
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster(&nets, 1).await;
        c.put("b", "k", b"v1".to_vec(), None, &Cond::None).await.unwrap();
        nets[1].net.push_fault("pin_object", Fault::ApplyThenFail);
        c.put("b", "k", b"v2".to_vec(), None, &Cond::None).await.unwrap();
        assert!(c.store(1).unwrap().dirty());
        c.delete("b", "k").await.unwrap();
        let r = maintain(&c).await;
        assert!(r.reconciled >= 1 && r.floors >= 2 && r.purged >= 1 && r.reclaimed >= 2, "{r:?}");
        for n in &nets {
            assert_eq!(on_disk(n, "k").await, (None, true, false), "payload reclaimed, the floor kept");
        }
        assert!(cluster(&nets, 1).await.lookup("b", "k").is_none());
    }

    #[tokio::test]
    async fn a_write_that_lands_after_maintenance_and_a_restart_stays_deleted() {
        // The regression the time-based GC failed: an abandoned pin of an older version is applied by the indexer
        // only after the DELETE, after every replica was re-read and its payload reclaimed, and after a restart.
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster_with(&nets, 1, short()).await;
        c.put("b", "k", b"v1".to_vec(), None, &Cond::None).await.unwrap();
        // keep r1's v1 object, to replay it later as a stale or rolled-back indexer would
        let stale = {
            let s = c.store(1).unwrap();
            let idx = s.idx.lock().await;
            let host = idx.keys[&("b".to_string(), "k".to_string())].host.clone();
            idx.objs[&host].0.clone()
        };
        nets[1].net.push_fault("pin_object", Fault::Defer);
        c.put("b", "k", b"v2".to_vec(), None, &Cond::None).await.unwrap(); // r1 abandoned after the grace
        assert!(c.store(1).unwrap().dirty());
        c.delete("b", "k").await.unwrap();
        maintain(&c).await;
        for n in &nets {
            assert_eq!(on_disk(n, "k").await, (None, true, false));
        }

        // now the indexer applies the v2 pin it was given long ago, and replays v1
        assert_eq!(nets[1].net.apply_deferred(), 1);
        c.store(1).unwrap().sdk.pin_object(&stale).await.unwrap();
        assert_eq!(on_disk(&nets[1], "k").await, (None, true, true), "both old versions are back on r1, under the floor");

        // a restart from durable state alone still sees the key deleted; its maintenance purges the old versions
        drop(c);
        let restarted = cluster(&nets, 1).await;
        assert!(restarted.lookup("b", "k").is_none());
        let r = maintain(&restarted).await;
        assert!(r.purged >= 1, "{r:?}");
        for n in &nets {
            assert_eq!(on_disk(n, "k").await, (None, true, false));
        }
        assert!(cluster(&nets, 1).await.lookup("b", "k").is_none());
    }

    #[tokio::test]
    async fn a_replica_rolled_back_to_before_a_delete_cannot_bring_the_key_back() {
        // fail-stop is the model, but floors on every replica also outlast up to N - W rolled-back indexers
        let mut nets = vec![Net::new(1), Net::new(2), Net::new(3)];
        let c = cluster(&nets, 2).await;
        c.put("b", "k", b"old".to_vec(), None, &Cond::None).await.unwrap();
        let stale = {
            let s = c.store(2).unwrap();
            let idx = s.idx.lock().await;
            let host = idx.keys[&("b".to_string(), "k".to_string())].host.clone();
            idx.objs[&host].0.clone()
        };
        c.delete("b", "k").await.unwrap();
        maintain(&c).await;
        drop(c);
        // r2's indexer comes back from a backup taken before the DELETE: no tombstone, no floor, the old version
        nets[2] = Net::new(3);
        let sdk = nets[2].net.sdk(nets[2].key.clone()).await.unwrap();
        sdk.pin_object(&stale).await.unwrap();
        assert_eq!(on_disk(&nets[2], "k").await.0, Some(stale_ver(&stale)));
        // every boot set (2 of 3) includes a replica with the floor
        for down in 0..2 {
            nets[down].offline(true);
            assert!(cluster(&nets, 2).await.lookup("b", "k").is_none(), "with r{down} down");
            nets[down].offline(false);
        }
        // and maintenance puts the floor back on r2 and purges what it replayed
        let all = cluster(&nets, 2).await;
        maintain(&all).await;
        assert_eq!(on_disk(&nets[2], "k").await, (None, true, false));
    }

    fn stale_ver(o: &Object) -> Ver {
        serde_json::from_slice::<crate::store::Meta>(&o.metadata).unwrap().e[0].ver()
    }

    #[tokio::test]
    async fn a_hung_replica_delays_a_quorum_write_by_the_grace_period_only() {
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster_with(&nets, 1, short()).await;
        // the same write with both replicas healthy, for scale (mock uploads are CPU-bound, so load varies)
        let t0 = std::time::Instant::now();
        c.put("b", "base", b"w".to_vec(), None, &Cond::None).await.unwrap();
        let base = t0.elapsed();
        // an upload that never completes on r1
        nets[1].net.push_fault("pin_slabs", Fault::Hang);
        let t0 = std::time::Instant::now();
        c.put("b", "a", b"x".to_vec(), None, &Cond::None).await.unwrap();
        let upload_wait = t0.elapsed();
        // a metadata commit that never answers on r1
        nets[1].net.push_fault("pin_object", Fault::Hang);
        let t0 = std::time::Instant::now();
        c.put("b", "b", b"y".to_vec(), None, &Cond::None).await.unwrap();
        let commit_wait = t0.elapsed();
        // bounded by the healthy replica's own time plus the grace period, not by the hung replica (forever)
        let bound = base * 3 + Duration::from_secs(5);
        assert!(upload_wait < bound && commit_wait < bound, "{base:?}: {upload_wait:?} {commit_wait:?}");
        assert!(c.store(1).unwrap().dirty(), "the abandoned pin's outcome is unknown");
        assert!(c.stats.borrow().abandoned_calls >= 2);
        // the write lock is free: the next write proceeds, and repair brings r1 up
        c.put("b", "c", b"z".to_vec(), None, &Cond::None).await.unwrap();
        maintain(&c).await;
        for k in ["base", "a", "b", "c"] {
            assert_eq!(on_disk(&nets[1], k).await.0, c.lookup("b", k).map(|e| e.ver()), "{k}");
        }

        // with W = 2 the same hang fails the write at the deadline instead of holding it forever
        let c2 = cluster_with(&nets, 2, short()).await;
        nets[1].net.push_fault("pin_object", Fault::Hang);
        let t0 = std::time::Instant::now();
        let e = c2.put("b", "d", b"w".to_vec(), None, &Cond::None).await.unwrap_err();
        assert!(e.starts_with("ServiceUnavailable"), "{e}");
        assert!(t0.elapsed() < base * 3 + c2.meta_deadline() + Duration::from_secs(5), "{base:?}: {:?}", t0.elapsed());
        assert!(c2.lookup("b", "d").is_some(), "r0 committed: visible, as after an S3 500");
    }

    #[tokio::test]
    async fn writes_continue_through_an_indexer_outage_and_repair_backfills() {
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster(&nets, 1).await;
        assert_eq!((c.n(), c.read_quorum()), (2, 2));
        c.put("b", "k1", b"first".to_vec(), None, &Cond::None).await.unwrap();

        nets[1].offline(true);
        c.put("b", "k2", b"written during the outage".to_vec(), None, &Cond::None).await.unwrap();
        assert_eq!(get(&c, "k2").await.unwrap(), b"written during the outage");
        assert!(c.replicas[1].error.borrow().is_some());

        nets[1].offline(false);
        let rep = maintain(&c).await;
        assert_eq!(rep.copies, 1);
        // the backfilled replica alone now serves the outage write, with the same version
        let want = c.lookup("b", "k2").unwrap().ver();
        assert_eq!(on_disk(&nets[1], "k2").await.0, Some(want));
        nets[0].offline(true);
        nets[0].net.clear_sectors();
        let only_b = cluster(&nets, 1).await;
        assert_eq!(only_b.loaded().len(), 1);
        let e = only_b.lookup("b", "k2").unwrap();
        let mut out = Vec::new();
        only_b.read(&e, 0, e.s, &mut out).await.unwrap();
        assert_eq!(out, b"written during the outage");
        assert!(!only_b.ready(), "one of two replicas is below the boot quorum when W = 1");
    }

    #[tokio::test]
    async fn quorum_refuses_and_leaves_nothing_visible() {
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster(&nets, 2).await;
        nets[1].offline(true);
        let e = c.put("b", "k", b"x".to_vec(), None, &Cond::None).await.unwrap_err();
        assert!(e.starts_with("ServiceUnavailable"), "{e}");
        assert!(c.lookup("b", "k").is_none());
        nets[1].offline(false);
        assert_eq!(on_disk(&nets[0], "k").await.0, None, "the staged upload on the healthy replica was never pinned");
        assert_eq!(c.stats.borrow().refused_writes, 1);
    }

    #[tokio::test]
    async fn a_missed_delete_never_resurrects_and_its_payload_is_reclaimed() {
        let nets = [Net::new(1), Net::new(2), Net::new(3)];
        let c = cluster(&nets, 2).await;
        c.put("b", "k", b"doomed".to_vec(), None, &Cond::None).await.unwrap();
        nets[2].offline(true);
        c.delete("b", "k").await.unwrap();
        assert!(c.lookup("b", "k").is_none());

        // a fresh boot that loads the replica which missed the DELETE still sees it (any 2 of 3 include a record)
        nets[1].offline(true);
        nets[2].offline(false);
        let boot = cluster(&nets, 2).await;
        assert!(boot.ready());
        assert!(boot.lookup("b", "k").is_none(), "r0's deletion hides r2's stale copy");
        assert!(on_disk(&nets[2], "k").await.0.is_some());

        // with all three back, maintenance applies the delete, writes floors everywhere and reclaims the payload
        nets[1].offline(false);
        let all = cluster(&nets, 2).await;
        let rep = maintain(&all).await;
        assert!(rep.deletes == 1 && rep.floors >= 3 && rep.reclaimed >= 3, "{rep:?}");
        for n in &nets {
            assert_eq!(on_disk(n, "k").await, (None, true, false));
        }
        // and a key put again afterwards is ordinary: newer than its floor
        all.put("b", "k", b"again".to_vec(), None, &Cond::IfNoneMatch).await.unwrap();
        assert_eq!(get(&all, "k").await.unwrap(), b"again");
        assert_eq!(get(&cluster(&nets, 2).await, "k").await.unwrap(), b"again");
    }

    #[tokio::test]
    async fn delete_succeeds_only_when_recorded_durably_on_a_write_quorum() {
        let nets = [Net::new(1), Net::new(2), Net::new(3)];
        let c = cluster(&nets, 2).await;
        c.put("b", "k", b"everywhere".to_vec(), None, &Cond::None).await.unwrap();
        nets[1].offline(true);
        nets[2].offline(true);
        // only r0 can record it
        let e = c.delete("b", "k").await.unwrap_err();
        assert!(e.starts_with("ServiceUnavailable"), "{e}");
        assert!(c.lookup("b", "k").is_none(), "the indeterminate delete is visible, as with an S3 500");
        nets[2].offline(false);
        c.delete("b", "k").await.unwrap(); // the retry records the same deletion on r2
        assert!(on_disk(&nets[2], "k").await.1);

        // a replica that never held the key records the deletion as a ledger floor
        nets[1].offline(false);
        nets[2].offline(true);
        c.put("b", "lag", b"r0 and r1".to_vec(), None, &Cond::None).await.unwrap();
        nets[2].offline(false);
        nets[1].offline(true);
        c.delete("b", "lag").await.unwrap();
        assert_eq!(on_disk(&nets[2], "lag").await, (None, true, false));
    }

    #[tokio::test]
    async fn boot_merges_the_newest_version_and_conditions_use_it() {
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster(&nets, 1).await;
        let old = c.put("b", "k", b"v1".to_vec(), None, &Cond::None).await.unwrap();
        nets[0].offline(true);
        let new = c.put("b", "k", b"v2".to_vec(), None, &Cond::IfMatch(old.clone())).await.unwrap();
        nets[0].offline(false);

        let boot = cluster(&nets, 1).await;
        assert_eq!(boot.lookup("b", "k").unwrap().t, new);
        assert_eq!(get(&boot, "k").await.unwrap(), b"v2");
        let e = boot.put("b", "k", b"v3".to_vec(), None, &Cond::IfMatch(old)).await.unwrap_err();
        assert_eq!(e, "PreconditionFailed");
        assert_eq!(maintain(&boot).await.copies, 1);
        assert_eq!(on_disk(&nets[0], "k").await.0, Some(boot.lookup("b", "k").unwrap().ver()));
        // the boot clock starts after every version it loaded, so a later write always orders last
        assert!(boot.clock.get() >= boot.lookup("b", "k").unwrap().ver().h);
        let v2 = boot.lookup("b", "k").unwrap().ver();
        boot.put("b", "k", b"v4".to_vec(), None, &Cond::IfMatch(new)).await.unwrap();
        assert!(boot.lookup("b", "k").unwrap().ver() > v2);
    }

    #[tokio::test]
    async fn reads_fail_over_to_a_replica_that_still_has_the_bytes() {
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster(&nets, 2).await;
        let body: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        c.put("b", "k", body.clone(), None, &Cond::None).await.unwrap();
        nets[0].net.clear_sectors(); // r0's hosts lost the data; its indexer still lists it
        let e = c.lookup("b", "k").unwrap();
        let mut out = Vec::new();
        assert_eq!(c.read(&e, 1000, 5000, &mut out).await.unwrap(), 5000);
        assert_eq!(out, &body[1000..6000]);
        nets[1].net.clear_sectors();
        assert!(c.read(&e, 0, 10, &mut Vec::new()).await.is_err());
    }

    #[tokio::test]
    async fn streamed_puts_tee_to_every_replica_and_drop_a_failing_one() {
        let nets = [Net::new(1), Net::new(2)];
        let c = cluster(&nets, 1).await;
        nets[1].offline(true);
        let body = vec![9u8; 100_000];
        let staged = c.upload_reader(std::io::Cursor::new(body.clone())).await.unwrap();
        assert_eq!(staged.objs.len(), 1);
        c.commit("b", "big", staged, md5_hex(&body), body.len() as u64, None, &Cond::IfNoneMatch).await.unwrap();
        assert_eq!(get(&c, "big").await.unwrap(), body);
        nets[1].offline(false);
        // with W = 2 both replicas must take the stream
        let c2 = cluster(&nets, 2).await;
        let staged = c2.upload_reader(std::io::Cursor::new(body.clone())).await.unwrap();
        assert_eq!(staged.objs.len(), 2);
        let e = c2.commit("b", "big", staged, md5_hex(&body), body.len() as u64, None, &Cond::IfNoneMatch).await.unwrap_err();
        assert_eq!(e, "PreconditionFailed");
    }

    /// The disk-backed mock answers exactly as the in-memory one: the same complete bytes and ranges through the
    /// real SDK (encryption, erasure coding, sector roots), the same failure once hosts lose their sectors, files
    /// only under its own directory and named only by hashes, an enforced byte budget, and full cleanup on drop.
    #[tokio::test]
    async fn disk_and_memory_mock_sectors_answer_alike() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let mem = [Net::new(7)];
        let dsk = [Net::on_disk(7)];
        let dir = dsk[0].net.sector_dir().expect("a disk-backed network has a sector directory");
        assert!(dir.starts_with(sector_base()));
        assert!(mem[0].net.sector_dir().is_none() && mem[0].net.sector_bytes().is_none());
        {
            let cm = cluster(&mem, 1).await;
            let cd = cluster(&dsk, 1).await;
            for c in [&cm, &cd] {
                c.put("b", "k", body.clone(), None, &Cond::None).await.unwrap();
            }
            for c in [&cm, &cd] {
                assert_eq!(get(c, "k").await.unwrap(), body);
                let e = c.lookup("b", "k").unwrap();
                for (off, len) in [(0u64, 1u64), (1, 4095), (4096, 60_000), (299_999, 1), (123_457, 100_000)] {
                    let mut out = Vec::new();
                    c.read(&e, off, len, &mut out).await.unwrap();
                    assert_eq!(out, &body[off as usize..(off + len) as usize], "range {off}+{len}");
                }
            }
            // what is on disk: one directory per host, one file per sector, all named by hex hashes, all inside `dir`
            let held = dsk[0].net.sector_bytes().unwrap();
            assert!(held > 0 && held % (4 << 20) == 0, "{held}");
            let mut files = 0u64;
            for h in std::fs::read_dir(&dir).unwrap() {
                let h = h.unwrap();
                let name = h.file_name().into_string().unwrap();
                assert!(name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()), "{name}");
                for f in std::fs::read_dir(h.path()).unwrap() {
                    let f = f.unwrap();
                    let n = f.file_name().into_string().unwrap();
                    let stem = n.strip_suffix(".sector").expect("only finished sectors remain");
                    assert!(stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()), "{n}");
                    assert_eq!(f.metadata().unwrap().len(), 4 << 20);
                    files += 1;
                }
            }
            assert_eq!(files * (4 << 20), held);
            // hosts losing every sector: both backends fail the read, with the same error
            mem[0].net.clear_sectors();
            dsk[0].net.clear_sectors();
            assert_eq!(dsk[0].net.sector_bytes(), Some(0));
            let em = cm.lookup("b", "k").unwrap();
            let ed = cd.lookup("b", "k").unwrap();
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let ra = cm.read(&em, 0, em.s, &mut a).await.unwrap_err();
            let rb = cd.read(&ed, 0, ed.s, &mut b).await.unwrap_err();
            assert_eq!(ra, rb);
        }
        // the budget is enforced: a write over it fails rather than vanishing
        let tight = [Net::on_disk_with(8, 4 << 20)];
        let ct = cluster(&tight, 1).await;
        assert!(ct.put("b", "k", body.clone(), None, &Cond::None).await.is_err());
        drop(ct);
        drop(tight);
        // and dropping the network (and every SDK built from it) removes its directory
        drop(dsk);
        assert!(!dir.exists(), "{} was left behind", dir.display());
    }

    #[tokio::test]
    async fn many_part_uploads_and_long_keys_use_manifests_everywhere() {
        // disk-backed: this test pins dozens of objects on two indexers, every one a full 12-shard slab of 4 MiB
        // sectors, which in RAM exceeds 2 GiB; on disk the identical upload, manifest and recovery paths run
        let nets = [Net::on_disk(1), Net::on_disk(2)];
        let c = cluster(&nets, 2).await;
        // 16 small parts: more than an inline entry can list (about 12) without large bodies
        let uid = c.create_upload("b", "mp").await.unwrap();
        let mut want = Vec::new();
        let mut body: Vec<u8> = Vec::new();
        for n in 1..=16u32 {
            let part = vec![n as u8; 1000 + n as usize];
            body.extend(&part);
            want.push((n, c.upload_part(&uid, n, part).await.unwrap()));
        }
        let (_, _, etag) = c.complete_upload(&uid, &want).await.unwrap();
        assert!(etag.ends_with("-16"));
        assert!(c.uploads().await.is_empty());
        assert_eq!(get(&c, "mp").await.unwrap(), body);
        // a range across part boundaries
        let e = c.lookup("b", "mp").unwrap();
        let mut out = Vec::new();
        c.read(&e, 1500, 3000, &mut out).await.unwrap();
        assert_eq!(out, &body[1500..4500]);

        // a key near S3's 1 KiB limit, and copies of both onto manifests
        let long = format!("dir/{}", "k".repeat(1000));
        c.put("b", &long, b"long".to_vec(), Some("text/plain".into()), &Cond::IfNoneMatch).await.unwrap();
        c.copy(("b", "mp"), ("b", "mp-copy")).await.unwrap();
        c.copy(("b", &long), ("b", "long-copy")).await.unwrap();

        // a fresh load of each indexer reads the manifests back
        for n in &nets {
            let s = n.load().await.unwrap();
            let idx = s.idx.lock().await;
            for k in ["mp", long.as_str(), "mp-copy", "long-copy"] {
                assert!(idx.keys[&("b".to_string(), k.to_string())].manifest, "{k}");
            }
            assert!(idx.unresolved.is_empty());
        }
        let boot = cluster(&nets, 2).await;
        assert_eq!(get(&boot, "mp-copy").await.unwrap(), body);
        assert_eq!(get(&boot, &long).await.unwrap(), b"long");
        assert_eq!(get(&boot, "long-copy").await.unwrap(), b"long");
        assert_eq!(boot.lookup("b", "mp").unwrap().t, etag);
        assert_eq!(boot.lookup("b", &long).unwrap().c.as_deref(), Some("text/plain"));

        // deleting the source leaves the copy whole; deleting everything reclaims every payload object
        boot.delete("b", "mp").await.unwrap();
        assert_eq!(get(&boot, "mp-copy").await.unwrap(), body);
        for k in ["mp-copy", long.as_str(), "long-copy"] {
            boot.delete("b", k).await.unwrap();
        }
        maintain(&boot).await;
        for n in &nets {
            let s = n.load().await.unwrap();
            let idx = s.idx.lock().await;
            assert!(idx.keys.is_empty() && idx.tombs.is_empty() && idx.manifests.is_empty() && idx.oldest.is_empty());
            assert_eq!(idx.floors.len(), 4);
            assert_eq!(idx.objs.len(), idx.ledgers.len(), "only ledgers remain");
        }
    }

    #[tokio::test]
    async fn one_indexer_keeps_a_floor_and_reclaims_the_payload() {
        let one = [Net::new(4)];
        let c = cluster(&one, 1).await;
        c.put("b", "k", b"x".to_vec(), None, &Cond::None).await.unwrap();
        c.delete("b", "k").await.unwrap();
        {
            // the deletion is a tombstone in the payload's own object, which stays until a floor covers it
            let s = one[0].load().await.unwrap();
            let idx = s.idx.lock().await;
            let q = qkey("b", "k");
            assert!(idx.keys.is_empty() && idx.tombs.contains_key(&q) && idx.floors.is_empty());
            assert_eq!(idx.objs.len(), 1);
        }
        maintain(&c).await;
        {
            let s = one[0].load().await.unwrap();
            let idx = s.idx.lock().await;
            assert!(idx.keys.is_empty() && idx.tombs.is_empty() && idx.floors.contains_key(&qkey("b", "k")));
            assert_eq!(idx.objs.len(), idx.ledgers.len(), "the payload is gone; the floor stays");
        }
        c.put("b", "k", b"y".to_vec(), None, &Cond::None).await.unwrap();
        assert_eq!(get(&cluster(&one, 1).await, "k").await.unwrap(), b"y");
    }

    #[test]
    fn legacy_entries_order_by_mtime_and_new_ones_by_clock() {
        let legacy: Entry = serde_json::from_str(r#"{"b":"b","k":"k","s":1,"t":"x","m":100}"#).unwrap();
        assert_eq!(legacy.ver(), Ver { h: 100_000, i: String::new() });
        let mut newer = legacy.clone();
        newer.h = 100_000;
        newer.i = "00".into();
        assert!(newer.ver() > legacy.ver(), "a write id breaks a clock tie");
        let json = serde_json::to_string(&legacy).unwrap();
        assert!(!json.contains("\"h\"") && !json.contains("\"i\""), "unversioned entries serialize as before");
    }

    #[tokio::test]
    async fn fan_out_returns_after_quorum_and_grace_or_the_deadline() {
        let t0 = std::time::Instant::now();
        let jobs: Vec<(usize, Pin<Box<dyn Future<Output = Res<u32>>>>)> = vec![(0, Box::pin(async { Ok(1) })), (1, Box::pin(std::future::pending()))];
        let out = fan_out(jobs, 1, Duration::from_millis(50), Duration::from_secs(60), Result::is_ok).await;
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert!(matches!(out[0], (0, Some(Ok(1)))) && matches!(out[1], (1, None)));
        let t0 = std::time::Instant::now();
        let jobs: Vec<(usize, Pin<Box<dyn Future<Output = Res<u32>>>>)> = vec![(0, Box::pin(std::future::pending()))];
        let out = fan_out(jobs, 1, Duration::ZERO, Duration::from_millis(100), Result::is_ok).await;
        assert!(t0.elapsed() < Duration::from_secs(1) && out[0].1.is_none());
    }
}
