//! Maintenance: keep each repository's pack count logarithmic, and delete
//! packs a repack superseded once nothing can still be reading them.
//!
//! Every push stores one pack, so packs pile up. A geometric repack (factor
//! 2) merges the newest packs whenever one would not be at least twice the
//! size of everything newer than it. Merging is concatenation: git packs
//! are position-independent apart from OFS deltas, whose offsets are
//! relative and survive a copy that keeps each pack's entries together, so
//! entries are copied byte for byte and only the header count, the trailer
//! and the index offsets change. The copy streams from storage to storage
//! in slices taken while the server is otherwise idle.
//!
//! The merged pack replaces its sources in the manifest by compare-and-swap
//! (the sources must still be there, as a run); the sources are listed as
//! retired and deleted an hour later, after fetches that planned against
//! them have finished.

use crate::app::{now, App};
use crate::git::ingest::IdxEntry;
use crate::git::repo::PackInfo;
use crate::store::{PackMeta, Retired, Saved, Upload};
use serde_json::{json, Value};
use sha1::Digest;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// seconds a retired pack is kept (DEPOT_RETIRE_GRACE overrides, for tests)
fn grace() -> u64 {
    std::env::var("DEPOT_RETIRE_GRACE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600)
}

/// Seconds an unlisted pack must have existed before it counts as an orphan
/// (an upload whose push or repack never committed). A day: far longer than
/// any push in flight. DEPOT_ORPHAN_AGE overrides, for tests.
fn orphan_age() -> u64 {
    std::env::var("DEPOT_ORPHAN_AGE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(86_400)
}

fn sweep_every() -> Duration {
    Duration::from_secs(
        std::env::var("DEPOT_SWEEP_EVERY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
    )
}
const READ: u64 = 4 << 20;
const MIN_PACKS: usize = 4;
const SLICE: Duration = Duration::from_millis(30);

#[derive(Default)]
pub struct State {
    queue: VecDeque<String>,
    job: Option<Repack>,
    last_sweep: Option<Instant>,
    orphan_scans: std::collections::HashMap<String, Instant>,
    pub orphans_deleted: u64,
    pub done: u64,
    pub last: Option<String>,
}

struct Repack {
    repo: String,
    sources: Vec<PackMeta>,
    new_id: String,
    total: u64,
    upload: Upload,
    src: usize,
    off: u64,
    buf: Vec<u8>,
    produced: u64,
    hasher: sha1::Sha1,
    entries: Vec<IdxEntry>,
    started: Instant,
}

/// The newest packs to merge: extend while the next older pack is smaller
/// than twice what is gathered so far.
pub fn pick(packs: &[PackMeta]) -> Option<usize> {
    if packs.len() < MIN_PACKS {
        return None;
    }
    let n = packs.len();
    let mut sum = packs[n - 1].len;
    let mut start = n - 1;
    for j in (0..n - 1).rev() {
        if packs[j].len < 2 * sum {
            sum += packs[j].len;
            start = j;
        } else {
            break;
        }
    }
    (n - start >= 2).then_some(start)
}

pub fn after_push(app: &mut App, repo_id: &str) {
    if let Some(r) = app.repos.get(repo_id) {
        if pick(&r.m.packs).is_some() && !app.maint.queue.iter().any(|x| x == repo_id) {
            app.maint.queue.push_back(repo_id.to_string());
        }
    }
}

pub fn request(app: &mut App, repo: &str) -> Result<Value, String> {
    let id = app.open(repo)?.ok_or("no such repository")?;
    let r = &app.repos[&id];
    let would = pick(&r.m.packs).map(|s| r.m.packs.len() - s);
    if would.is_some() && !app.maint.queue.iter().any(|x| *x == id) {
        app.maint.queue.push_back(id.clone());
    }
    Ok(
        json!({ "packs": r.m.packs.len(), "would_merge": would, "retired": r.m.retired.len(), "queued": app.maint.queue.len() }),
    )
}

pub fn status(app: &App) -> Value {
    let job = app.maint.job.as_ref().map(|j| {
        json!({ "sources": j.sources.len(), "bytes": j.total, "copied": j.produced, "seconds": j.started.elapsed().as_secs() })
    });
    json!({ "queued": app.maint.queue.len(), "running": job, "repacks": app.maint.done, "last": app.maint.last,
            "orphans_deleted": app.maint.orphans_deleted })
}

/// One slice of background work; true when it did anything.
pub fn tick(app: &mut App) -> bool {
    if app.maint.job.is_some() {
        let mut job = app.maint.job.take().unwrap();
        match step(app, &mut job) {
            Ok(true) => {
                app.maint.done += 1;
                app.maint.last = Some(format!(
                    "merged {} packs ({:.1} MiB) in {:.0}s",
                    job.sources.len(),
                    job.total as f64 / 1048576.0,
                    job.started.elapsed().as_secs_f64()
                ));
                eprintln!("[depot] repack: {}", app.maint.last.as_ref().unwrap());
            }
            Ok(false) => app.maint.job = Some(job),
            Err(e) => {
                eprintln!("[depot] repack abandoned: {e}");
                app.store.upload_abort(&job.upload);
                // the merged pack goes only if a fresh manifest provably does
                // not list it; unknown means it stays for the orphan sweep
                let listed = app
                    .revalidate(&job.repo, true)
                    .ok()
                    .and_then(|_| app.repos.get(&job.repo))
                    .map(|r| r.m.lists(&job.new_id));
                if listed == Some(false) {
                    let _ = app.store.delete_pack(&job.repo, &job.new_id);
                }
                app.maint.last = Some(format!("abandoned: {e}"));
            }
        }
        return true;
    }
    if let Some(id) = app.maint.queue.pop_front() {
        match start(app, &id) {
            Ok(Some(j)) => app.maint.job = Some(j),
            Ok(None) => {}
            Err(e) => eprintln!("[depot] repack not started: {e}"),
        }
        return true;
    }
    if app
        .maint
        .last_sweep
        .map_or(true, |t| t.elapsed() > sweep_every())
    {
        app.maint.last_sweep = Some(Instant::now());
        let ids: Vec<String> = app.repos.keys().cloned().collect();
        for id in ids {
            if let Err(e) = sweep(app, &id) {
                eprintln!("[depot] retired-pack sweep: {e}");
            }
            let due =
                app.maint.orphan_scans.get(&id).is_none_or(|t| {
                    t.elapsed() > Duration::from_secs(orphan_age().clamp(1, 86_400))
                });
            if due {
                app.maint.orphan_scans.insert(id.clone(), Instant::now());
                if let Err(e) = orphans(app, &id) {
                    eprintln!("[depot] orphan sweep: {e}");
                }
            }
            // repositories loaded since the last push get their merge too
            after_push(app, &id);
        }
        return true;
    }
    false
}

fn start(app: &mut App, id: &str) -> Result<Option<Repack>, String> {
    app.revalidate(id, true)?;
    let r = app.repos.get(id).ok_or("repository unloaded")?;
    let Some(s) = pick(&r.m.packs) else {
        return Ok(None);
    };
    let sources: Vec<PackMeta> = r.m.packs[s..].to_vec();
    let mut entries = Vec::new();
    let mut base = 12u64;
    let mut count = 0u64;
    for p in &sources {
        for mut e in app.store.get_idx(id, &p.id)? {
            e.offset = e.offset - 12 + base;
            entries.push(e);
        }
        base += p.len - 32;
        count += p.objects as u64;
    }
    let total = base + 20;
    if count > u32::MAX as u64 {
        return Err("too many objects to merge".into());
    }
    let new_id = crate::seal::random_id();
    let upload = app.store.upload_begin(id, &new_id, total)?;
    let mut buf = Vec::with_capacity(12);
    buf.extend_from_slice(b"PACK");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&(count as u32).to_be_bytes());
    let mut hasher = sha1::Sha1::new();
    hasher.update(&buf);
    eprintln!(
        "[depot] repack {}: merging {} packs ({:.1} MiB)",
        r.name,
        sources.len(),
        total as f64 / 1048576.0
    );
    Ok(Some(Repack {
        repo: id.to_string(),
        sources,
        new_id,
        total,
        upload,
        src: 0,
        off: 12,
        produced: 12,
        buf,
        hasher,
        entries,
        started: Instant::now(),
    }))
}

fn step(app: &mut App, j: &mut Repack) -> Result<bool, String> {
    let t0 = Instant::now();
    while t0.elapsed() < SLICE {
        let (a, b) = app.store.next_part(&j.upload);
        // fill the buffer up to the next part's end
        if j.produced < b {
            if j.src < j.sources.len() {
                let p = &j.sources[j.src];
                let end = p.len - 20;
                let take = (end - j.off).min(READ).min(b - j.produced);
                if take > 0 {
                    let info = PackInfo {
                        id: p.id.clone(),
                        len: p.len,
                        chunk: p.chunk,
                    };
                    let data = app
                        .store
                        .read_pack_uncached(&j.repo, &info, j.off, j.off + take)?;
                    j.hasher.update(&data);
                    j.buf.extend_from_slice(&data);
                    j.produced += take;
                    j.off += take;
                }
                if j.off == end {
                    j.src += 1;
                    j.off = 12;
                }
                continue;
            }
            // all entries copied: the trailer
            let d: [u8; 20] = std::mem::take(&mut j.hasher).finalize().into();
            j.buf.extend_from_slice(&d);
            j.produced += 20;
            if j.produced != j.total {
                return Err(format!(
                    "merged length {} != planned {}",
                    j.produced, j.total
                ));
            }
            continue;
        }
        // a full part is buffered (buf starts at `a`)
        let n = (b - a) as usize;
        let part: Vec<u8> = j.buf.drain(..n).collect();
        if app.store.upload_part(&mut j.upload, &part)? {
            app.store.put_idx(&j.repo, &j.new_id, &j.entries)?;
            commit(app, j)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn commit(app: &mut App, j: &Repack) -> Result<(), String> {
    let ids: Vec<&str> = j.sources.iter().map(|p| p.id.as_str()).collect();
    for attempt in 0..8 {
        if attempt > 0 {
            app.revalidate(&j.repo, true)?;
        }
        let r = app.repos.get(&j.repo).ok_or("repository unloaded")?;
        if r.m.lists(&j.new_id) {
            return Ok(()); // an earlier attempt landed
        }
        let pos =
            r.m.packs
                .iter()
                .position(|p| p.id == ids[0])
                .ok_or("a source pack is gone (repacked elsewhere?)")?;
        if r.m.packs.len() < pos + ids.len()
            || r.m.packs[pos..pos + ids.len()]
                .iter()
                .zip(&ids)
                .any(|(p, id)| p.id != *id)
        {
            return Err("source packs are no longer a run in the manifest".into());
        }
        let mut m = r.m.clone();
        let merged = PackMeta {
            id: j.new_id.clone(),
            len: j.total,
            objects: j.entries.len() as u32,
            chunk: crate::seal::CHUNK,
        };
        // objects: the header count (every entry) equals the index length here
        m.packs.splice(pos..pos + ids.len(), [merged]);
        let at = now();
        m.retired.extend(ids.iter().map(|id| Retired {
            id: id.to_string(),
            at,
        }));
        let etag = r.etag.clone();
        match app
            .store
            .save_manifest(&j.repo, &mut m, etag.as_deref(), at)?
        {
            Saved::Ok(t) => {
                // committed: a failure to re-index now is repaired by the next
                // revalidation, never by undoing the merge
                if let Err(e) = app.apply(&j.repo, m, t) {
                    eprintln!("[depot] repack committed; re-indexing deferred: {e}");
                    app.mark_stale(&j.repo);
                }
                return Ok(());
            }
            Saved::Conflict => continue,
        }
    }
    Err("manifest busy".into())
}

/// Delete pack objects no manifest lists (live or retired) that are older
/// than `orphan_age()`: the uploads of pushes and repacks that never committed.
fn orphans(app: &mut App, id: &str) -> Result<(), String> {
    app.revalidate(id, true)?;
    let Some(r) = app.repos.get(id) else {
        return Ok(());
    };
    let mut known: std::collections::HashSet<String> =
        r.m.packs.iter().map(|p| p.id.clone()).collect();
    known.extend(r.m.retired.iter().map(|x| x.id.clone()));
    if let Some(j) = &app.maint.job {
        known.insert(j.new_id.clone());
    }
    let cutoff = now().saturating_sub(orphan_age());
    let mut gone = 0;
    for (name, modified) in app.store.list_repo(id)? {
        let Some(pack) = name
            .strip_suffix(".pack")
            .or_else(|| name.strip_suffix(".idx"))
        else {
            continue;
        };
        if known.contains(pack) || modified > cutoff || modified == 0 {
            continue;
        }
        app.store.delete_object(&format!("r/{id}/{name}"))?;
        gone += 1;
    }
    if gone > 0 {
        app.maint.orphans_deleted += gone;
        eprintln!("[depot] deleted {gone} orphaned pack objects");
    }
    Ok(())
}

/// Delete packs retired more than an hour ago, then drop them from the list.
fn sweep(app: &mut App, id: &str) -> Result<(), String> {
    if !app.repos.get(id).is_some_and(|r| !r.m.retired.is_empty()) {
        return Ok(());
    }
    app.revalidate(id, true)?;
    let Some(r) = app.repos.get(id) else {
        return Ok(());
    };
    let cutoff = now().saturating_sub(grace());
    let candidates: Vec<String> =
        r.m.retired
            .iter()
            .filter(|x| x.at <= cutoff && !r.m.packs.iter().any(|p| p.id == x.id))
            .map(|x| x.id.clone())
            .collect();
    // never a pack the manifest still serves, nor one a fetch in flight here
    // still reads (it holds the pack's record)
    let old: Vec<String> = candidates.into_iter().filter(|p| !app.held(p)).collect();
    if old.is_empty() {
        return Ok(());
    }
    for p in &old {
        app.store.delete_pack(id, p)?;
    }
    for attempt in 0..8 {
        if attempt > 0 {
            app.revalidate(id, true)?;
        }
        let r = &app.repos[id];
        let mut m = r.m.clone();
        m.retired.retain(|x| !old.contains(&x.id));
        if m.retired.len() == r.m.retired.len() {
            return Ok(());
        }
        let etag = r.etag.clone();
        match app
            .store
            .save_manifest(id, &mut m, etag.as_deref(), now())?
        {
            Saved::Ok(t) => {
                let r = app.repos.get_mut(id).unwrap();
                r.m = m;
                r.etag = Some(t);
                return Ok(());
            }
            Saved::Conflict => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(len: u64) -> PackMeta {
        PackMeta {
            id: len.to_string(),
            len,
            objects: 1,
            chunk: 1,
        }
    }

    #[test]
    fn geometric_choice() {
        assert_eq!(pick(&[p(1000), p(10), p(10)]), None, "too few packs");
        assert_eq!(
            pick(&[p(1000), p(400), p(100), p(10), p(10)]),
            Some(3),
            "only the small tail"
        );
        assert_eq!(
            pick(&[p(1000), p(100), p(50), p(25), p(12)]),
            None,
            "already geometric"
        );
        assert_eq!(pick(&[p(100), p(90), p(80), p(70)]), Some(0), "everything");
    }
}
