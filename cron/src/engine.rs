use crate::schedule::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
pub const GRACE: u64 = 30;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Eyesoff { target: String, prompt: String },
    Http { target: String, body: Value },
}
impl Action {
    pub fn target(&self) -> &str {
        match self {
            Self::Eyesoff { target, .. } | Self::Http { target, .. } => target,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
    pub schedule: Schedule,
    pub action: Action,
    pub client_key: String,
}
impl Spec {
    pub fn validate(&self, now: u64) -> Result<u64, String> {
        if self.name.trim().is_empty()
            || self.name.len() > 160
            || !crate::config::valid_id(&self.client_key)
        {
            return Err("Provide a name and an idempotent client_key (letters/numbers/-/_)".into());
        }
        if serde_json::to_vec(&self.action)
            .map_err(|_| "Invalid action")?
            .len()
            > 16384
        {
            return Err("Action exceeds 16 KiB".into());
        }
        if let Action::Eyesoff { prompt, .. } = &self.action {
            if prompt.trim().is_empty() {
                return Err("Prompt must not be empty".into());
            }
        }
        self.schedule.validate(now)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub owner: String,
    pub spec: Spec,
    pub enabled: bool,
    pub next_due: Option<u64>,
    pub generation: u64,
    pub created_at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub job_id: String,
    pub owner: String,
    pub due: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub status: String,
    pub result: Option<String>,
    pub http_status: Option<u16>,
    pub action: Action,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub revision: u64,
    pub lease_owner: String,
    pub lease_until: u64,
    pub jobs: BTreeMap<String, Job>,
    pub runs: Vec<Run>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            lease_owner: String::new(),
            lease_until: 0,
            jobs: BTreeMap::new(),
            runs: Vec::new(),
        }
    }
}
pub fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
pub fn random_id() -> Result<String, String> {
    let mut b = [0; 16];
    getrandom::getrandom(&mut b).map_err(|_| "Randomness unavailable")?;
    Ok(b.iter().map(|v| format!("{v:02x}")).collect())
}
impl State {
    pub fn create(&mut self, owner: &str, spec: Spec, now: u64) -> Result<Job, String> {
        if let Some(j) = self
            .jobs
            .values()
            .find(|j| j.owner == owner && j.spec.client_key == spec.client_key)
        {
            return if j.spec == spec {
                Ok(j.clone())
            } else {
                Err("client_key already used for a different job".into())
            };
        }
        let due = spec.validate(now)?;
        if self.jobs.len() >= 128 || self.jobs.values().filter(|j| j.owner == owner).count() >= 32 {
            return Err("Job quota reached (32 per user, 128 total)".into());
        }
        let id = random_id()?;
        let j = Job {
            id: id.clone(),
            owner: owner.into(),
            spec,
            enabled: true,
            next_due: Some(due),
            generation: 1,
            created_at: now,
        };
        self.jobs.insert(id, j.clone());
        Ok(j)
    }
    pub fn get(&self, owner: &str, id: &str) -> Result<&Job, String> {
        self.jobs
            .get(id)
            .filter(|j| j.owner == owner)
            .ok_or_else(|| "Job not found".into())
    }
    pub fn update(
        &mut self,
        owner: &str,
        id: &str,
        spec: Option<Spec>,
        enabled: Option<bool>,
        generation: u64,
        now: u64,
    ) -> Result<Job, String> {
        let current = self.get(owner, id)?;
        if current.generation != generation {
            return Err("Job changed; read it again before updating".into());
        }
        if let Some(s) = &spec {
            s.validate(now)?;
            if s.client_key != current.spec.client_key {
                return Err("client_key cannot change".into());
            }
        }
        let j = self.jobs.get_mut(id).unwrap();
        if let Some(s) = spec {
            j.spec = s;
            j.next_due = j.spec.schedule.next_after(now);
        }
        if let Some(e) = enabled {
            if e && !j.enabled {
                j.next_due = j.spec.schedule.next_after(now);
                if j.next_due.is_none() {
                    return Err("Expired one-time job; update its schedule first".into());
                }
            }
            j.enabled = e;
        }
        j.generation += 1;
        Ok(j.clone())
    }
    pub fn delete(&mut self, owner: &str, id: &str) -> Result<(), String> {
        self.get(owner, id)?;
        if self
            .runs
            .iter()
            .any(|r| r.job_id == id && r.status == "running")
        {
            return Err("Pause future runs first; this job currently has an active run".into());
        }
        self.jobs.remove(id);
        Ok(())
    }
    pub fn recover(&mut self, now: u64) {
        for r in &mut self.runs {
            if r.status == "running" {
                r.status = "interrupted".into();
                r.finished_at = Some(now);
                r.result=Some("Scheduler stopped during delivery; outcome may be unknown. Not automatically retried.".into());
            }
        }
        let ids: Vec<_> = self
            .jobs
            .values()
            .filter(|j| j.enabled && j.next_due.is_some_and(|t| t <= now))
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            self.skip(&id, now, "Missed while scheduler was offline");
        }
        self.trim();
    }
    fn skip(&mut self, id: &str, now: u64, reason: &str) {
        let j = self.jobs.get_mut(id).unwrap();
        let due = j.next_due.unwrap();
        let r = Run {
            id: hash(&format!("{id}:{}:{due}", j.generation)),
            job_id: id.into(),
            owner: j.owner.clone(),
            due,
            started_at: None,
            finished_at: Some(now),
            status: "skipped".into(),
            result: Some(reason.into()),
            http_status: None,
            action: j.spec.action.clone(),
        };
        j.next_due = j.spec.schedule.next_after(now);
        if j.next_due.is_none() {
            j.enabled = false;
        }
        self.runs.push(r);
    }
    pub fn claim_due(&mut self, now: u64, slots: usize) -> Vec<Run> {
        let mut ids: Vec<_> = self
            .jobs
            .values()
            .filter(|j| j.enabled && j.next_due.is_some_and(|t| t <= now))
            .map(|j| (j.next_due.unwrap(), j.id.clone()))
            .collect();
        ids.sort();
        let mut out = Vec::new();
        for (due, id) in ids {
            if self
                .runs
                .iter()
                .any(|r| r.job_id == id && r.status == "running")
            {
                self.skip(
                    &id,
                    now,
                    "Previous run still active; overlapping occurrence skipped",
                );
                continue;
            }
            if now.saturating_sub(due) > GRACE {
                self.skip(&id, now, "Occurrence missed its 30-second dispatch grace");
                continue;
            }
            if out.len() >= slots {
                continue;
            }
            let j = self.jobs.get_mut(&id).unwrap();
            let run = Run {
                id: hash(&format!("{id}:{}:{due}", j.generation)),
                job_id: id,
                owner: j.owner.clone(),
                due,
                started_at: Some(now),
                finished_at: None,
                status: "running".into(),
                result: None,
                http_status: None,
                action: j.spec.action.clone(),
            };
            j.next_due = j.spec.schedule.next_after(now);
            if j.next_due.is_none() {
                j.enabled = false;
            }
            self.runs.push(run.clone());
            out.push(run);
        }
        self.trim();
        out
    }
    pub fn run_now(&mut self, owner: &str, id: &str, key: &str, now: u64) -> Result<Run, String> {
        let j = self.get(owner, id)?.clone();
        if !crate::config::valid_id(key) {
            return Err("run_now requires a unique request_key".into());
        }
        let rid = hash(&format!("{id}:manual:{key}"));
        if let Some(r) = self.runs.iter().find(|r| r.id == rid) {
            return Ok(r.clone());
        }
        if self
            .runs
            .iter()
            .any(|r| r.job_id == id && r.status == "running")
        {
            return Err("Job already running".into());
        }
        let r = Run {
            id: rid,
            job_id: id.into(),
            owner: owner.into(),
            due: now,
            started_at: Some(now),
            finished_at: None,
            status: "running".into(),
            result: None,
            http_status: None,
            action: j.spec.action,
        };
        self.runs.push(r.clone());
        self.trim();
        Ok(r)
    }
    pub fn finish(
        &mut self,
        id: &str,
        now: u64,
        status: &str,
        result: String,
        http_status: Option<u16>,
    ) {
        if let Some(r) = self.runs.iter_mut().find(|r| r.id == id) {
            r.status = status.into();
            r.finished_at = Some(now);
            r.result = Some(truncate(&result, 8192));
            r.http_status = http_status;
        }
        self.trim();
    }
    fn trim(&mut self) {
        let mut counts = BTreeMap::new();
        self.runs.reverse();
        self.runs.retain(|r| {
            let n = counts.entry(r.owner.clone()).or_insert(0);
            *n += 1;
            r.status == "running" || *n <= 32
        });
        self.runs.reverse();
        while self.runs.len() > 256
            || serde_json::to_vec(self).map_or(true, |b| b.len() > 3 * 1024 * 1024)
        {
            if let Some(i) = self.runs.iter().position(|r| r.status != "running") {
                self.runs.remove(i);
            } else {
                break;
            }
        }
    }
}
pub fn truncate(s: &str, max: usize) -> String {
    let mut n = max.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].into()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> Spec {
        Spec {
            name: "test".into(),
            client_key: "a".into(),
            schedule: Schedule::Interval {
                seconds: 60,
                start_at: crate::schedule::iso(120),
            },
            action: Action::Eyesoff {
                target: "ai".into(),
                prompt: "hello".into(),
            },
        }
    }
    #[test]
    fn recovery_skips_and_does_not_replay() {
        let mut s = State::default();
        let j = s.create("a", spec(), 60).unwrap();
        let r = s.claim_due(120, 1);
        assert_eq!(r.len(), 1);
        s.recover(500);
        assert_eq!(s.runs[0].status, "interrupted");
        assert_eq!(s.runs[1].status, "skipped");
        assert_eq!(s.jobs[&j.id].next_due, Some(540));
        assert!(s.claim_due(500, 1).is_empty());
    }
    #[test]
    fn once_skips_on_restart() {
        let mut v = spec();
        v.schedule = Schedule::Once {
            at: crate::schedule::iso(120),
        };
        let mut s = State::default();
        let j = s.create("a", v, 60).unwrap();
        s.recover(121);
        assert!(!s.jobs[&j.id].enabled);
        assert!(s.claim_due(122, 1).is_empty());
    }
    #[test]
    fn scoped_idempotent_and_nonoverlap() {
        let mut s = State::default();
        let j = s.create("a", spec(), 60).unwrap();
        assert_eq!(s.create("a", spec(), 70).unwrap().id, j.id);
        assert!(s.get("b", &j.id).is_err());
        assert!(s.delete("b", &j.id).is_err());
        s.claim_due(120, 1);
        assert!(s.claim_due(180, 1).is_empty());
        assert_eq!(s.runs.last().unwrap().status, "skipped");
    }
    #[test]
    fn stale_and_missed() {
        let mut s = State::default();
        let j = s.create("a", spec(), 60).unwrap();
        assert!(s.update("a", &j.id, None, Some(false), 2, 80).is_err());
        assert!(s.claim_due(160, 1).is_empty());
        assert_eq!(s.runs[0].status, "skipped");
        assert_eq!(s.jobs[&j.id].next_due, Some(180));
    }
    #[test]
    fn manual_idempotence() {
        let mut s = State::default();
        let j = s.create("a", spec(), 60).unwrap();
        let r = s.run_now("a", &j.id, "x", 70).unwrap();
        assert_eq!(s.run_now("a", &j.id, "x", 75).unwrap().id, r.id);
        assert!(s.run_now("b", &j.id, "x", 75).is_err());
    }
}
