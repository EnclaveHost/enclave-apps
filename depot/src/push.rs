//! A push, end to end: the request body streams into the receiver (commands,
//! then the pack into the indexer); once it ends, the response is a job
//! that resolves deltas, checks connectivity and every command, seals and
//! uploads the pack and its index, and commits the new refs with a
//! compare-and-swap on the manifest, reporting progress on side-band 2 the
//! whole way (which also keeps the platform's idle timers fed).

use crate::app::{now, App, RepoIo};
use crate::config::Who;
use crate::git::ingest::{IdxEntry, Ingest, Limits};
use crate::git::pkt;
use crate::git::receive::{self, Caps, Command, Incoming, Receiver};
use crate::git::repo::Index;
use crate::git::segbuf::SegBuf;
use crate::serve::{Response, Sink, Source};
use crate::store::{Manifest, PackMeta, RepoMeta, Saved, Upload};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const RESULT_CT: &str = "application/x-git-receive-pack-result";
const SLICE: Duration = Duration::from_millis(40);
const DISCARD_CAP: u64 = 4 << 30;

/// Bytes of pushed packs held in memory across every push in flight, so
/// two large pushes at once cannot exhaust the guest's memory. A charge is
/// released when its holder (the sink, then the job) is dropped, however the
/// request ends.
#[derive(Clone, Default)]
pub struct Budget(Rc<Cell<u64>>);

impl Budget {
    pub fn held(&self) -> u64 {
        self.0.get()
    }
}

pub struct Charge {
    budget: Budget,
    n: u64,
}

impl Charge {
    fn add(&mut self, k: u64, cap: u64) -> Result<(), String> {
        if self.budget.0.get() + k > cap {
            return Err(format!(
                "the server is holding other pushes ({} MiB); try again shortly",
                (self.budget.0.get() - self.n) >> 20
            ));
        }
        self.budget.0.set(self.budget.0.get() + k);
        self.n += k;
        Ok(())
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budget
            .0
            .set(self.budget.0.get().saturating_sub(self.n));
    }
}

pub struct PushSink {
    pub repo: String,
    pub who: Who,
    rx: Receiver,
    failed: Option<String>,
    discarded: u64,
    charge: Charge,
    cap: u64,
}

impl PushSink {
    pub fn new(repo: &str, who: Who, limits: Limits, budget: Budget) -> PushSink {
        let cap = limits.max_pack + (64 << 20);
        PushSink {
            repo: repo.to_string(),
            who,
            rx: Receiver::new(limits),
            failed: None,
            discarded: 0,
            charge: Charge { budget, n: 0 },
            cap,
        }
    }
}

fn report_response(
    caps: &Caps,
    unpack: Result<(), &str>,
    results: &[(String, Result<(), String>)],
    prefix: Vec<u8>,
) -> Response<App> {
    let rep = receive::report(unpack, results);
    let mut body = prefix;
    if caps.sideband {
        if let Err(e) = unpack {
            pkt::band(
                &mut body,
                2,
                format!("error: {e}\n").as_bytes(),
                pkt::MAX_BAND,
            );
        }
        pkt::band(&mut body, 1, &rep, pkt::MAX_BAND);
        pkt::flush(&mut body);
    } else {
        body.extend_from_slice(&rep);
    }
    Response::new(200)
        .with("cache-control", "no-cache")
        .bytes(RESULT_CT, body)
}

impl Sink<App> for PushSink {
    fn data(&mut self, _app: &mut App, d: &[u8]) -> Result<(), String> {
        if self.failed.is_some() {
            // keep reading so the client receives the report, not a reset
            self.discarded += d.len() as u64;
            if self.discarded > DISCARD_CAP {
                return Err("push refused".into());
            }
            return Ok(());
        }
        if let Err(e) = self
            .charge
            .add(d.len() as u64, self.cap)
            .and_then(|_| self.rx.feed(d))
        {
            self.failed = Some(e);
        }
        Ok(())
    }

    fn end(mut self: Box<Self>, app: &mut App) -> Response<App> {
        if self.failed.is_none() {
            if let Err(e) = self.rx.finish() {
                self.failed = Some(e);
            }
        }
        let names: Vec<(String, Result<(), String>)> = self
            .rx
            .cmds
            .iter()
            .map(|c| (c.name.clone(), Err("unpacker error".to_string())))
            .collect();
        if let Some(e) = &self.failed {
            app.stats.failures += 1;
            eprintln!(
                "[depot] push to {} by {} refused: {e}",
                self.repo,
                self.who.name()
            );
            return report_response(&self.rx.caps, Err(e), &names, Vec::new());
        }
        if self.rx.cmds.is_empty() {
            // git's authentication probe before a large push: a bare flush
            return Response::new(200)
                .with("cache-control", "no-cache")
                .bytes(RESULT_CT, Vec::new());
        }
        let rx = std::mem::replace(
            &mut self.rx,
            Receiver::new(Limits {
                max_pack: 0,
                max_object: 0,
            }),
        );
        let charge = std::mem::replace(
            &mut self.charge,
            Charge {
                budget: Budget::default(),
                n: 0,
            },
        );
        app.stats.push_bytes += rx.bytes;
        let job = PushJob {
            repo: self.repo.clone(),
            who: self.who.clone(),
            caps: rx.caps,
            cmds: rx.cmds,
            ingest: rx.ingest,
            done: None,
            stage: Stage::Start,
            upload: None,
            pack_id: crate::seal::random_id(),
            repo_id: None,
            results: Vec::new(),
            started: Instant::now(),
            last_progress: None,
            _charge: charge,
        };
        Response::new(200)
            .with("cache-control", "no-cache")
            .stream(RESULT_CT, Box::new(job))
    }

    fn fail(self: Box<Self>, app: &mut App, err: String) -> Response<App> {
        app.stats.failures += 1;
        eprintln!("[depot] push to {} failed: {err}", self.repo);
        let names: Vec<(String, Result<(), String>)> = self
            .rx
            .cmds
            .iter()
            .map(|c| (c.name.clone(), Err("unpacker error".to_string())))
            .collect();
        report_response(&self.rx.caps, Err(&err), &names, Vec::new())
    }
}

enum Stage {
    Start,
    Resolve,
    Check,
    Upload,
    Commit,
    Report(Result<(), String>),
    Finished,
}

pub struct PushJob {
    repo: String,
    who: Who,
    caps: Caps,
    cmds: Vec<Command>,
    ingest: Option<Ingest>,
    done: Option<(SegBuf, Vec<IdxEntry>)>,
    stage: Stage,
    upload: Option<Upload>,
    pack_id: String,
    repo_id: Option<String>,
    results: Vec<(String, Result<(), String>)>,
    started: Instant,
    last_progress: Option<Instant>,
    _charge: Charge,
}

impl PushJob {
    fn progress(&mut self, out: &mut Vec<u8>, msg: &str, force: bool) {
        if !self.caps.sideband || self.caps.quiet {
            return;
        }
        if force
            || self
                .last_progress
                .map_or(true, |t| t.elapsed() >= Duration::from_millis(500))
        {
            self.last_progress = Some(Instant::now());
            pkt::band(out, 2, msg.as_bytes(), pkt::MAX_BAND);
        }
    }

    fn step(&mut self, app: &mut App, out: &mut Vec<u8>) -> Result<(), String> {
        match &mut self.stage {
            Stage::Start => {
                self.repo_id = app.open(&self.repo)?;
                if let Some(i) = &self.ingest {
                    let n = i.objects();
                    self.progress(out, &format!("depot: indexing {n} objects\n"), true);
                }
                self.stage = Stage::Resolve;
            }
            Stage::Resolve => {
                let Some(ing) = self.ingest.as_mut() else {
                    self.stage = Stage::Check;
                    return Ok(());
                };
                let deadline = Instant::now() + SLICE;
                let empty = Index::default();
                let App {
                    store,
                    repos,
                    objects,
                    cfg,
                    ..
                } = app;
                let (rid, ix) = match self
                    .repo_id
                    .as_deref()
                    .and_then(|id| repos.get(id).map(|r| (id, &r.ix)))
                {
                    Some((id, ix)) => (id.to_string(), ix),
                    None => (String::new(), &empty),
                };
                let mut io = RepoIo {
                    store,
                    repo: &rid,
                    ix,
                    cache: objects,
                    max: cfg.max_object,
                };
                let finished = ing.resolve(deadline, &mut io)?;
                let (n, t) = (ing.resolved_deltas, ing.total_deltas);
                if finished {
                    let ing = self.ingest.take().unwrap();
                    self.done = Some(ing.complete()?);
                    if t > 0 {
                        self.progress(
                            out,
                            &format!("Resolving deltas: 100% ({t}/{t}), done.\n"),
                            true,
                        );
                    }
                    self.stage = Stage::Check;
                } else if t > 0 {
                    self.progress(
                        out,
                        &format!("Resolving deltas: {:3}% ({n}/{t})\r", n * 100 / t.max(1)),
                        false,
                    );
                }
            }
            Stage::Check => {
                let entries: &[IdxEntry] =
                    self.done.as_ref().map(|d| d.1.as_slice()).unwrap_or(&[]);
                let inc = Incoming::new(entries);
                let empty_ix = Index::default();
                let empty_refs = Default::default();
                let (ix, refs) = match self.repo_id.as_deref().and_then(|id| app.repos.get(id)) {
                    Some(r) => (&r.ix, &r.refs),
                    None => (&empty_ix, &empty_refs),
                };
                inc.connected(ix)?;
                // a first judgement against the refs as they are now; the
                // commit judges again against the refs it replaces
                let pre: Vec<(String, Result<(), String>)> = self
                    .cmds
                    .iter()
                    .map(|c| {
                        (
                            c.name.clone(),
                            receive::check(
                                c,
                                refs.get(&c.name),
                                &inc,
                                ix,
                                app.cfg.is_protected(&c.name),
                            ),
                        )
                    })
                    .collect();
                let any_ok = pre.iter().any(|(_, r)| r.is_ok());
                let all_ok = pre.iter().all(|(_, r)| r.is_ok());
                if !any_ok || (self.caps.atomic && !all_ok) {
                    self.results = atomic_results(pre, self.caps.atomic);
                    self.stage = Stage::Report(Ok(()));
                    return Ok(());
                }
                if self.repo_id.is_none() && !self.cmds.iter().any(|c| !c.new.is_zero()) {
                    return Err("repository does not exist".into());
                }
                if self.repo_id.is_none() {
                    self.repo_id = Some(crate::seal::random_id());
                }
                let objects = self.done.as_ref().map(|d| d.1.len()).unwrap_or(0);
                if objects > 0 {
                    let len = self.done.as_ref().unwrap().0.len() as u64;
                    self.upload = Some(app.store.upload_begin(
                        self.repo_id.as_ref().unwrap(),
                        &self.pack_id,
                        len,
                    )?);
                    self.progress(
                        out,
                        &format!(
                            "depot: sealing {objects} objects ({:.1} MiB) into storage\n",
                            len as f64 / 1048576.0
                        ),
                        true,
                    );
                    self.stage = Stage::Upload;
                } else {
                    self.stage = Stage::Commit;
                }
            }
            Stage::Upload => {
                let up = self.upload.as_mut().unwrap();
                let (bytes, entries) = self.done.as_ref().unwrap();
                let (a, b) = app.store.next_part(up);
                let finished = app
                    .store
                    .upload_part(up, &bytes.slice(a as usize, b as usize))?;
                let (sent, total) = (up.sent, bytes.len() as u64);
                if finished {
                    app.store
                        .put_idx(self.repo_id.as_ref().unwrap(), &self.pack_id, entries)?;
                    self.progress(
                        out,
                        &format!(
                            "Storing: 100% ({:.1}/{:.1} MiB), done.\n",
                            total as f64 / 1048576.0,
                            total as f64 / 1048576.0
                        ),
                        true,
                    );
                    self.stage = Stage::Commit;
                } else {
                    let msg = format!(
                        "Storing: {:3}% ({:.1}/{:.1} MiB)\r",
                        sent * 100 / total.max(1),
                        sent as f64 / 1048576.0,
                        total as f64 / 1048576.0
                    );
                    self.progress(out, &msg, false);
                }
            }
            Stage::Commit => {
                if let Err(e) = self.commit(app) {
                    eprintln!("[depot] push {}: commit failed: {e}", self.repo);
                    self.results = self
                        .cmds
                        .iter()
                        .map(|c| (c.name.clone(), Err(e.clone())))
                        .collect();
                }
                if !self.results.iter().any(|(_, x)| x.is_ok()) {
                    self.cleanup(app);
                }
                self.stage = Stage::Report(Ok(()));
            }
            Stage::Report(r) => {
                // Err: the pack itself was refused (unpack failed); every ref fails with it
                let unpack = r.clone();
                let results: Vec<(String, Result<(), String>)> = match &unpack {
                    Err(_) => self
                        .cmds
                        .iter()
                        .map(|c| (c.name.clone(), Err("unpacker error".to_string())))
                        .collect(),
                    Ok(()) => std::mem::take(&mut self.results),
                };
                let ok = results.iter().filter(|(_, r)| r.is_ok()).count();
                eprintln!(
                    "[depot] push {} by {}: {ok}/{} refs updated in {:.1}s{}",
                    self.repo,
                    self.who.name(),
                    results.len(),
                    self.started.elapsed().as_secs_f64(),
                    unpack
                        .as_ref()
                        .err()
                        .map(|e| format!(" (refused: {e})"))
                        .unwrap_or_default()
                );
                let rep = receive::report(
                    unpack.as_ref().map(|_| ()).map_err(|e| e.as_str()),
                    &results,
                );
                if self.caps.sideband {
                    if let Err(e) = &unpack {
                        pkt::band(out, 2, format!("error: {e}\n").as_bytes(), pkt::MAX_BAND);
                    }
                    pkt::band(out, 1, &rep, pkt::MAX_BAND);
                    pkt::flush(out);
                } else {
                    out.extend_from_slice(&rep);
                }
                app.stats.pushes += 1;
                self.done = None;
                self.stage = Stage::Finished;
            }
            Stage::Finished => {}
        }
        Ok(())
    }

    fn cleanup(&mut self, app: &mut App) {
        if let (Some(id), Some(up)) = (&self.repo_id, &self.upload) {
            app.store.upload_abort(up);
            let _ = app.store.delete_pack(id, &self.pack_id);
        }
    }

    /// Judge every command against the manifest it replaces and swap it in.
    fn commit(&mut self, app: &mut App) -> Result<(), String> {
        let id = self.repo_id.clone().unwrap();
        // a repository's first push registers it
        if !app.repos.contains_key(&id) {
            register(app, &self.repo, &id)?;
            app.open(&self.repo)?
                .filter(|x| *x == id)
                .ok_or("repository registration did not take")?;
        }
        let entries: Vec<IdxEntry> = self.done.as_ref().map(|d| d.1.clone()).unwrap_or_default();
        let inc = Incoming::new(&entries);
        let pack = self
            .done
            .as_ref()
            .filter(|d| !d.1.is_empty())
            .map(|d| PackMeta {
                id: self.pack_id.clone(),
                len: d.0.len() as u64,
                objects: d.1.len() as u32,
                chunk: crate::seal::CHUNK,
            });
        for attempt in 0..8 {
            if attempt > 0 {
                app.revalidate(&id, true)?;
            }
            let r = app
                .repos
                .get(&id)
                .ok_or("repository vanished during the push")?;
            let judged: Vec<(String, Result<(), String>)> = self
                .cmds
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        receive::check(
                            c,
                            r.refs.get(&c.name),
                            &inc,
                            &r.ix,
                            app.cfg.is_protected(&c.name),
                        ),
                    )
                })
                .collect();
            let all_ok = judged.iter().all(|(_, x)| x.is_ok());
            let any_ok = judged.iter().any(|(_, x)| x.is_ok());
            if !any_ok || (self.caps.atomic && !all_ok) {
                self.results = atomic_results(judged, self.caps.atomic);
                return Ok(());
            }
            let mut m: Manifest = r.m.clone();
            for (c, (_, res)) in self.cmds.iter().zip(&judged) {
                if res.is_ok() {
                    if c.new.is_zero() {
                        m.refs.remove(&c.name);
                    } else {
                        m.refs.insert(c.name.clone(), c.new.hex());
                    }
                }
            }
            if let Some(p) = &pack {
                if !m.packs.iter().any(|x| x.id == p.id) {
                    m.packs.push(p.clone());
                }
            }
            fix_head(&mut m, &app.cfg.default_branch);
            let etag = r.etag.clone();
            match app.store.save_manifest(&id, &mut m, etag.as_deref(), now()) {
                Ok(Saved::Ok(t)) => {
                    let r = app.repos.get_mut(&id).unwrap();
                    if let Some(p) = &pack {
                        let info = std::rc::Rc::new(crate::git::repo::PackInfo {
                            id: p.id.clone(),
                            len: p.len,
                            chunk: p.chunk,
                        });
                        r.ix.add_pack(info, &entries)?;
                    }
                    r.refs = crate::app::parse_refs(&m)?;
                    r.m = m;
                    r.etag = Some(t);
                    r.checked = Instant::now();
                    let updates: Vec<crate::hooks::Update> = self
                        .cmds
                        .iter()
                        .zip(&judged)
                        .filter(|(_, (_, r))| r.is_ok())
                        .map(|(c, _)| crate::hooks::Update {
                            name: &c.name,
                            old: c.old.hex(),
                            new: c.new.hex(),
                        })
                        .collect();
                    crate::hooks::push_event(app, &self.repo, self.who.name(), &updates);
                    self.results = judged;
                    crate::maint::after_push(app, &id);
                    return Ok(());
                }
                Ok(Saved::Conflict) => continue,
                Err(e) => {
                    // ambiguous: did it land?
                    let writer = m.writer.clone();
                    app.revalidate(&id, true)?;
                    let r = app.repos.get(&id).unwrap();
                    if r.m.writer == writer {
                        self.results = judged;
                        return Ok(());
                    }
                    return Err(format!("could not commit the push: {e}"));
                }
            }
        }
        Err("too many concurrent updates to this repository; push again".into())
    }
}

fn atomic_results(
    judged: Vec<(String, Result<(), String>)>,
    atomic: bool,
) -> Vec<(String, Result<(), String>)> {
    if !atomic {
        return judged;
    }
    judged
        .into_iter()
        .map(|(n, r)| match r {
            Ok(()) => (n, Err("atomic push failed".to_string())),
            Err(e) => (n, Err(e)),
        })
        .collect()
}

/// HEAD names a missing branch (a new repository, or its branch was
/// deleted): point it at the default branch, main, master, or the first.
pub fn fix_head(m: &mut Manifest, default_branch: &str) {
    if m.refs.contains_key(&m.head) {
        return;
    }
    for c in [
        format!("refs/heads/{default_branch}"),
        "refs/heads/main".into(),
        "refs/heads/master".into(),
    ] {
        if m.refs.contains_key(&c) {
            m.head = c;
            return;
        }
    }
    if let Some(first) = m.refs.keys().find(|k| k.starts_with("refs/heads/")) {
        m.head = first.clone();
    }
}

/// Add `name -> id` to the registry (compare-and-swap).
pub fn register(app: &mut App, name: &str, id: &str) -> Result<(), String> {
    for _ in 0..8 {
        app.refresh_registry(true)?;
        if let Some(m) = app.reg.repos.get(name) {
            if m.id == id {
                return Ok(());
            }
            return Err(
                "the repository was created concurrently by another push; push again".into(),
            );
        }
        let mut r = app.reg.clone();
        r.repos.insert(
            name.to_string(),
            RepoMeta {
                id: id.to_string(),
                created: now(),
                public: false,
                description: String::new(),
            },
        );
        let etag = app.reg_etag.clone();
        match app.store.save_registry(&mut r, etag.as_deref())? {
            Saved::Ok(t) => {
                app.reg = r;
                app.reg_etag = Some(t);
                return Ok(());
            }
            Saved::Conflict => continue,
        }
    }
    Err("registry busy; try again".into())
}

impl Source<App> for PushJob {
    fn pull(&mut self, app: &mut App, out: &mut Vec<u8>) -> Result<bool, String> {
        if let Stage::Finished = self.stage {
            return Ok(true);
        }
        if let Err(e) = self.step(app, out) {
            app.stats.failures += 1;
            eprintln!(
                "[depot] push to {} by {} failed: {e}",
                self.repo,
                self.who.name()
            );
            self.cleanup(app);
            self.stage = Stage::Report(Err(e));
        }
        Ok(matches!(self.stage, Stage::Finished))
    }
}
