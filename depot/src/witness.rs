//! The rollback witness: a second, independently operated store that
//! remembers the newest revision of every mutable document (the registry,
//! the token book, each repository's manifest).
//!
//! Sealing stops storage from forging or altering what depot wrote, but not
//! from serving an older copy it kept: an earlier manifest (a force push
//! undone, a deleted branch back), registry (a repository public again) or
//! token book (a revoked token working again). Within one process the
//! revision counters catch that; across a restart nothing in the bucket
//! can, since the bucket is what is lying. The witness can: every write
//! depot commits is followed by a write of `{rev, writer}` to the witness,
//! and the first load of a document after a start must find storage at
//! least as new as the witness says it was. Fooling depot then takes both
//! operators at once.
//!
//! Witness records are sealed like everything else (the witness learns
//! only that something changed) and only ever move forward: a write is
//! "raise to at least this revision", so retries and two servers writing
//! at once are harmless. When the witness cannot be written the document
//! stays committed and the record is retried in the background (`lag`),
//! so a rollback to a revision inside that window goes unseen. When it
//! cannot be read, the document is not loaded: rollback protection fails
//! closed.

use crate::s3::{Cond, S3};
use crate::seal::{self, Keys};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize)]
struct Record {
    v: u32,
    rev: u64,
    writer: String,
}

pub struct Witness {
    pub s3: S3,
    key: [u8; 32],
    /// storage's prefix, replaced by the witness's own in record keys
    from: String,
    to: String,
    /// newest record known per storage key: (rev, ETag of the record)
    known: HashMap<String, (u64, Option<String>)>,
    /// storage keys whose first load was checked
    checked: HashSet<String>,
    /// records not yet written: storage key -> (rev, writer)
    pub lag: BTreeMap<String, (u64, String)>,
    /// documents storage serves older than witnessed: storage key ->
    /// (what storage holds, if anything: rev and writer; the witnessed rev)
    pub rolled: BTreeMap<String, (Option<(u64, String)>, u64)>,
    pub writes: u64,
    pub failures: u64,
    /// writes are not attempted until then (the witness just failed)
    down_until: Option<Instant>,
}

fn compress(b: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(b, 6)
}

impl Witness {
    pub fn new(s3: S3, keys: &Keys, storage_prefix: &str, prefix: &str) -> Witness {
        Witness {
            s3,
            key: keys.derive("witness"),
            from: storage_prefix.to_string(),
            to: prefix.to_string(),
            known: HashMap::new(),
            checked: HashSet::new(),
            lag: BTreeMap::new(),
            rolled: BTreeMap::new(),
            writes: 0,
            failures: 0,
            down_until: None,
        }
    }

    fn wkey(&self, key: &str) -> String {
        format!("{}{}", self.to, key.strip_prefix(&self.from).unwrap_or(key))
    }

    /// The witnessed revision of `key` (None: never witnessed).
    fn read(&mut self, key: &str) -> Result<Option<(u64, String)>, String> {
        let wk = self.wkey(key);
        let Some(g) = self.s3.get(&wk, None)? else {
            self.known.remove(key);
            return Ok(None);
        };
        let plain = seal::open(&self.key, wk.as_bytes(), &g.body)?;
        let plain = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&plain, 1 << 20)
            .map_err(|_| "witness record does not decompress")?;
        let r: Record =
            serde_json::from_slice(&plain).map_err(|e| format!("witness record: {e}"))?;
        self.known.insert(key.to_string(), (r.rev, g.etag));
        Ok(Some((r.rev, r.writer)))
    }

    fn sealed(&self, wk: &str, rev: u64, writer: &str) -> Vec<u8> {
        let rec = Record {
            v: 1,
            rev,
            writer: writer.to_string(),
        };
        seal::seal(
            &self.key,
            wk.as_bytes(),
            &compress(&serde_json::to_vec(&rec).unwrap()),
        )
    }

    /// Raise the record of `key` to `rev` (no-op when it is already there).
    fn raise(&mut self, key: &str, rev: u64, writer: &str) -> Result<(), String> {
        if self.down_until.is_some_and(|t| Instant::now() < t) {
            return Err("unreachable a moment ago".into());
        }
        let r = self.raise_now(key, rev, writer);
        if r.is_err() {
            self.down_until = Some(Instant::now() + Duration::from_secs(30));
        } else {
            self.down_until = None;
        }
        r
    }

    fn raise_now(&mut self, key: &str, rev: u64, writer: &str) -> Result<(), String> {
        let wk = self.wkey(key);
        let body = self.sealed(&wk, rev, writer);
        for _ in 0..6 {
            let (cur, etag) = match self.known.get(key) {
                Some((r, t)) => (Some(*r), t.clone()),
                None => (self.read(key)?.map(|x| x.0), self.known.get(key).and_then(|k| k.1.clone())),
            };
            if cur.is_some_and(|c| c >= rev) {
                return Ok(());
            }
            let cond = match (&cur, &etag) {
                (Some(_), Some(t)) => Cond::IfMatch(t),
                (Some(_), None) => Cond::None, // a store without ETags: last writer wins
                (None, _) => Cond::IfNoneMatch,
            };
            let r = match self.s3.put(&wk, &body, cond) {
                // a store without conditional writes: last writer wins
                Err(e) if e.contains("HTTP 501") => self.s3.put(&wk, &body, Cond::None),
                r => r,
            };
            match r {
                Ok(Ok(t)) => {
                    self.writes += 1;
                    self.known.insert(key.to_string(), (rev, t));
                    return Ok(());
                }
                // refused, or the outcome is unknown: read what is there now
                Ok(Err(_)) | Err(_) => {
                    self.known.remove(key);
                }
            }
        }
        Err("witness busy".into())
    }

    /// A document was committed at `rev`: witness it, or queue the record.
    pub fn note(&mut self, key: &str, rev: u64, writer: &str) {
        self.checked.insert(key.to_string());
        if let Err(e) = self.raise(key, rev, writer) {
            self.failures += 1;
            eprintln!("[depot] witness: cannot record {key} at revision {rev} ({e}); will retry");
            let e = self
                .lag
                .entry(key.to_string())
                .or_insert((rev, writer.to_string()));
            if e.0 < rev {
                *e = (rev, writer.to_string());
            }
        } else if self.lag.get(key).is_some_and(|l| l.0 <= rev) {
            self.lag.remove(key);
        }
    }

    /// Retry one queued record (every few seconds at most); true when it did.
    pub fn retry(&mut self) -> bool {
        if self.lag.is_empty() || self.down_until.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        let Some((k, (rev, w))) = self.lag.pop_first() else {
            return false;
        };
        if self.raise(&k, rev, &w).is_err() {
            self.lag.insert(k, (rev, w));
        }
        true
    }

    /// The first load of `key` in this process found `got` (rev, writer),
    /// or nothing: refuse it if the witness has seen newer.
    pub fn check(&mut self, key: &str, got: Option<(u64, &str)>) -> Result<(), String> {
        if self.checked.contains(key) {
            return Ok(());
        }
        let seen = self
            .read(key)
            .map_err(|e| format!("the rollback witness cannot be read ({e}); not loading {key}"))?;
        let bad = match (&seen, got) {
            (None, _) => false,
            (Some((w, _)), None) => *w > 0,
            (Some((w, ww)), Some((r, rw))) => r < *w || (r == *w && ww != rw),
        };
        if bad {
            let w = seen.as_ref().unwrap().0;
            self.rolled.insert(
                key.to_string(),
                (got.map(|(r, rw)| (r, rw.to_string())), w),
            );
            return Err(match got {
                Some((r, _)) => format!(
                    "storage serves revision {r} of {key}, but revision {w} was committed (rollback?); \
                     not loading it. If storage really lost data, an admin can accept what it holds: POST /api/witness"
                ),
                None => format!(
                    "{key} is missing from storage, but revision {w} was committed (rollback?); \
                     not loading it. If storage really lost data, an admin can accept that: POST /api/witness"
                ),
            });
        }
        self.rolled.remove(key);
        self.checked.insert(key.to_string());
        Ok(())
    }

    /// An admin accepts what storage holds for every document found rolled
    /// back: the witness records are lowered to match (or removed). Returns
    /// the keys accepted.
    pub fn accept(&mut self) -> Result<Vec<String>, String> {
        let mut done = Vec::new();
        while let Some((k, (got, w))) = self.rolled.pop_first() {
            let wk = self.wkey(&k);
            let r = match &got {
                Some((rev, writer)) => {
                    let body = self.sealed(&wk, *rev, writer);
                    self.s3.put(&wk, &body, Cond::None).and_then(|r| {
                        r.map(|_| ())
                            .map_err(|s| format!("witness refused the write (HTTP {s})"))
                    })
                }
                None => self.s3.delete(&wk),
            };
            if let Err(e) = r {
                self.rolled.insert(k, (got, w));
                return Err(e);
            }
            eprintln!("[depot] witness: an admin accepted storage's copy of {k} (witnessed revision {w})");
            self.known.remove(&k);
            done.push(k);
        }
        Ok(done)
    }

    /// A document was deleted on purpose (a repository's manifest).
    pub fn forget(&mut self, key: &str) {
        let wk = self.wkey(key);
        let _ = self.s3.delete(&wk);
        self.known.remove(key);
        self.lag.remove(key);
        self.checked.remove(key);
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "writes": self.writes,
            "failures": self.failures,
            "lagging": self.lag.keys().collect::<Vec<_>>(),
            "rolled_back": self.rolled.iter().map(|(k, (got, w))| serde_json::json!({
                "key": k, "storage": got.as_ref().map(|g| g.0), "witnessed": w })).collect::<Vec<_>>(),
        })
    }
}
