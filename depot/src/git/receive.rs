//! receive-pack: the push side. The request is parsed as it streams in
//! (commands as pkt-lines, optional push options, then the pack, handed to
//! the indexer chunk by chunk). Checks: every object the pack brings links
//! only to objects it brings or the repository holds, with the right
//! types; branch heads are commits; a protected ref only fast-forwards and
//! is never deleted; each command's old value is the ref's value when the
//! update commits.

use super::ingest::{IdxEntry, Ingest, Limits};
use super::pkt::{self, Pkt};
use super::repo::{Index, OidMap, NONE};
use super::upload::{advertise_refs, View, AGENT};
use super::{valid_refname, Kind, Oid};
use std::collections::BTreeMap;

pub fn advertise(v: &View) -> Vec<u8> {
    let caps = format!(
        "report-status delete-refs side-band-64k quiet atomic ofs-delta push-options object-format=sha1 agent={AGENT}"
    );
    advertise_refs(v, &caps, false, false)
}

#[derive(Clone, Debug)]
pub struct Command {
    pub old: Oid,
    pub new: Oid,
    pub name: String,
}

#[derive(Default)]
pub struct Caps {
    pub sideband: bool,
    pub report: bool,
    pub atomic: bool,
    pub quiet: bool,
    pub push_options: bool,
}

enum Stage {
    Commands,
    Options,
    Pack,
}

pub struct Receiver {
    stage: Stage,
    pending: Vec<u8>,
    pub cmds: Vec<Command>,
    pub caps: Caps,
    pub options: Vec<String>,
    pub ingest: Option<Ingest>,
    limits: Option<Limits>,
    pub bytes: u64,
}

impl Receiver {
    pub fn new(limits: Limits) -> Receiver {
        Receiver {
            stage: Stage::Commands,
            pending: Vec::new(),
            cmds: Vec::new(),
            caps: Caps::default(),
            options: Vec::new(),
            ingest: None,
            limits: Some(limits),
            bytes: 0,
        }
    }

    pub fn needs_pack(&self) -> bool {
        self.cmds.iter().any(|c| !c.new.is_zero())
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<(), String> {
        self.bytes += data.len() as u64;
        if let Stage::Pack = self.stage {
            return self.feed_pack(data);
        }
        self.pending.extend_from_slice(data);
        loop {
            let parsed = pkt::parse(&self.pending)?;
            let Some((p, n)) = parsed else {
                if self.pending.len() > 1 << 20 {
                    return Err("push command section too large".into());
                }
                return Ok(());
            };
            let line = match &p {
                Pkt::Flush => None,
                Pkt::Data(d) => Some(d.to_vec()),
                _ => return Err("unexpected pkt in push request".into()),
            };
            self.pending.drain(..n);
            match (&self.stage, line) {
                (Stage::Commands, Some(d)) => self.command(&d)?,
                (Stage::Commands, None) => {
                    self.stage = if self.caps.push_options {
                        Stage::Options
                    } else {
                        Stage::Pack
                    };
                    if let Stage::Pack = self.stage {
                        return self.enter_pack();
                    }
                }
                (Stage::Options, Some(d)) => {
                    if self.options.len() < 64 {
                        self.options
                            .push(String::from_utf8_lossy(&d).trim_end().to_string());
                    }
                }
                (Stage::Options, None) => {
                    self.stage = Stage::Pack;
                    return self.enter_pack();
                }
                (Stage::Pack, _) => unreachable!(),
            }
        }
    }

    fn enter_pack(&mut self) -> Result<(), String> {
        let rest = std::mem::take(&mut self.pending);
        if self.needs_pack() {
            self.ingest = Some(Ingest::new(self.limits.take().unwrap()));
        }
        if !rest.is_empty() {
            self.feed_pack(&rest)?;
        }
        Ok(())
    }

    fn feed_pack(&mut self, data: &[u8]) -> Result<(), String> {
        match &mut self.ingest {
            Some(i) => i.feed(data),
            None if data.is_empty() => Ok(()),
            None => {
                // deletes only: git sends no pack, but tolerate an empty one
                if self.limits.is_some() {
                    self.ingest = Some(Ingest::new(self.limits.take().unwrap()));
                    self.ingest.as_mut().unwrap().feed(data)
                } else {
                    Err("unexpected data after push commands".into())
                }
            }
        }
    }

    fn command(&mut self, d: &[u8]) -> Result<(), String> {
        let (line, caps) = match d.iter().position(|&b| b == 0) {
            Some(i) => (&d[..i], Some(&d[i + 1..])),
            None => (d, None),
        };
        let line = std::str::from_utf8(line).map_err(|_| "push command is not text")?;
        let line = line.trim_end_matches('\n');
        if line.starts_with("shallow ") {
            return Ok(()); // a shallow client; connectivity decides whether the push stands
        }
        if line.starts_with("push-cert") {
            return Err("signed pushes are not supported".into());
        }
        if let Some(c) = caps {
            for cap in String::from_utf8_lossy(c).split_whitespace() {
                match cap {
                    "side-band-64k" => self.caps.sideband = true,
                    "report-status" | "report-status-v2" => self.caps.report = true,
                    "atomic" => self.caps.atomic = true,
                    "quiet" => self.caps.quiet = true,
                    "push-options" => self.caps.push_options = true,
                    _ => {}
                }
            }
        }
        let mut it = line.splitn(3, ' ');
        let (Some(o), Some(n), Some(name)) = (it.next(), it.next(), it.next()) else {
            return Err(format!("malformed push command: {line}"));
        };
        let old = Oid::from_hex(o).ok_or("bad old object id in push command")?;
        let new = Oid::from_hex(n).ok_or("bad new object id in push command")?;
        if self.cmds.len() >= 10_000 {
            return Err("too many ref updates in one push".into());
        }
        self.cmds.push(Command {
            old,
            new,
            name: name.to_string(),
        });
        Ok(())
    }

    /// The body ended.
    pub fn finish(&mut self) -> Result<(), String> {
        match self.stage {
            Stage::Pack => {}
            _ if self.cmds.is_empty() && self.pending.is_empty() => return Ok(()), // a bare flush: git's auth probe
            _ => return Err("push request ended early".into()),
        }
        if self.needs_pack() && self.ingest.is_none() {
            return Err("push request carries no pack".into());
        }
        if let Some(i) = &mut self.ingest {
            i.finish_input()?;
        }
        Ok(())
    }
}

/// The objects a push brings, by id: kind and links.
pub struct Incoming {
    pub map: OidMap<(Kind, Vec<(Kind, Oid)>)>,
}

impl Incoming {
    pub fn new(entries: &[IdxEntry]) -> Incoming {
        let mut map = OidMap::default();
        for e in entries {
            map.entry(e.oid)
                .or_insert_with(|| (e.kind, e.children.clone()));
        }
        Incoming { map }
    }

    pub fn kind(&self, ix: &Index, oid: &Oid) -> Option<Kind> {
        self.map
            .get(oid)
            .map(|e| e.0)
            .or_else(|| ix.lookup(oid).map(|i| ix.obj(i).kind))
    }

    /// Every link of every incoming object resolves, to the right type.
    pub fn connected(&self, ix: &Index) -> Result<(), String> {
        for (oid, (_, kids)) in &self.map {
            for (k, c) in kids {
                match self.kind(ix, c) {
                    Some(got) if got == *k => {}
                    Some(got) => {
                        return Err(format!(
                            "object {oid} links {c} as a {} but it is a {}",
                            k.name(),
                            got.name()
                        ))
                    }
                    None => return Err(format!("missing object {c} (linked from {oid})")),
                }
            }
        }
        Ok(())
    }

    fn parents(&self, ix: &Index, c: &Oid) -> Vec<Oid> {
        if let Some((_, kids)) = self.map.get(c) {
            return kids.iter().skip(1).map(|(_, o)| *o).collect();
        }
        match ix.lookup(c) {
            Some(i) => ix
                .kids(i)
                .iter()
                .skip(1)
                .filter(|&&p| p != NONE)
                .map(|&p| ix.oid(p))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Is `old` an ancestor of (or equal to) `new`, across incoming and stored commits?
    pub fn is_ancestor(&self, ix: &Index, old: &Oid, new: &Oid) -> bool {
        if old == new {
            return true;
        }
        let mut seen = std::collections::HashSet::new();
        let mut st = vec![*new];
        while let Some(c) = st.pop() {
            if c == *old {
                return true;
            }
            if !seen.insert(c) {
                continue;
            }
            if !self.map.contains_key(&c) {
                // stored history: walk it in the index
                if let (Some(a), Some(b)) = (ix.lookup(old), ix.lookup(&c)) {
                    if super::walk::is_ancestor(ix, a, b) {
                        return true;
                    }
                }
                continue;
            }
            st.extend(self.parents(ix, &c));
        }
        false
    }
}

/// Judge one command against the ref's current value.
pub fn check(
    cmd: &Command,
    current: Option<&Oid>,
    inc: &Incoming,
    ix: &Index,
    protected: bool,
) -> Result<(), String> {
    if !valid_refname(&cmd.name) {
        return Err("funny refname".into());
    }
    let cur = current.copied().unwrap_or_default();
    if cur != cmd.old {
        // the ref already holds what this command asks for (a retried push,
        // or the same update arriving twice): nothing left to do
        if cur == cmd.new && !(cmd.new.is_zero() && cmd.old.is_zero()) {
            return Ok(());
        }
        return Err("stale info".into());
    }
    if cmd.new.is_zero() {
        if cmd.old.is_zero() {
            return Err("nothing to delete".into());
        }
        if protected {
            return Err("deletion of a protected ref".into());
        }
        return Ok(());
    }
    let kind = inc.kind(ix, &cmd.new).ok_or("missing necessary objects")?;
    if cmd.name.starts_with("refs/heads/") && kind != Kind::Commit {
        return Err("branch tips must be commits".into());
    }
    if protected && !cmd.old.is_zero() {
        let ff = kind == Kind::Commit
            && inc.kind(ix, &cmd.old) == Some(Kind::Commit)
            && inc.is_ancestor(ix, &cmd.old, &cmd.new);
        if !ff {
            return Err("non-fast-forward (protected ref)".into());
        }
    }
    Ok(())
}

/// Judge every command of a push: each against its ref's current value
/// (`check`), then the survivors together, since git cannot store a ref and
/// another ref beneath it (`refs/heads/a` and `refs/heads/a/b`): a push that
/// would leave both makes the repository unclonable.
pub fn judge(
    cmds: &[Command],
    refs: &BTreeMap<String, Oid>,
    inc: &Incoming,
    ix: &Index,
    protected: impl Fn(&str) -> bool,
) -> Vec<(String, Result<(), String>)> {
    let mut out: Vec<(String, Result<(), String>)> = cmds
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                check(c, refs.get(&c.name), inc, ix, protected(&c.name)),
            )
        })
        .collect();
    let mut after = refs.clone();
    for (c, (_, r)) in cmds.iter().zip(&out) {
        if r.is_ok() {
            if c.new.is_zero() {
                after.remove(&c.name);
            } else {
                after.insert(c.name.clone(), c.new);
            }
        }
    }
    for (c, (_, r)) in cmds.iter().zip(out.iter_mut()) {
        if r.is_ok() && !c.new.is_zero() {
            if let Some(other) = df_conflict(&c.name, &after) {
                *r = Err(format!("cannot coexist with {other}"));
            }
        }
    }
    out
}

/// A ref that would clash with `name` as directory and file.
pub fn df_conflict(name: &str, refs: &BTreeMap<String, Oid>) -> Option<String> {
    for (i, _) in name.match_indices('/') {
        if refs.contains_key(&name[..i]) {
            return Some(name[..i].to_string());
        }
    }
    let dir = format!("{name}/");
    refs.range(dir.clone()..)
        .next()
        .filter(|(k, _)| k.starts_with(&dir))
        .map(|(k, _)| k.clone())
}

/// report-status: `unpack ok|<err>`, then `ok <ref>` / `ng <ref> <why>`, flush.
pub fn report(unpack: Result<(), &str>, results: &[(String, Result<(), String>)]) -> Vec<u8> {
    let mut b = Vec::new();
    match unpack {
        Ok(()) => pkt::line(&mut b, "unpack ok\n"),
        Err(e) => pkt::line(&mut b, &format!("unpack {}\n", one_line(e))),
    }
    for (name, r) in results {
        match r {
            Ok(()) => pkt::line(&mut b, &format!("ok {name}\n")),
            Err(e) => pkt::line(&mut b, &format!("ng {name} {}\n", one_line(e))),
        }
    }
    pkt::flush(&mut b);
    b
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::walk::tests::linear;

    #[test]
    fn parses_commands_then_pack_across_feeds() {
        let mut body = Vec::new();
        pkt::line(
            &mut body,
            &format!(
                "{} {} refs/heads/main\0report-status side-band-64k atomic\n",
                Oid([1; 20]),
                Oid([2; 20])
            ),
        );
        pkt::line(
            &mut body,
            &format!("{} {} refs/heads/old\n", Oid([3; 20]), crate::git::ZERO),
        );
        pkt::flush(&mut body);
        let pack = crate::git::ingest::tests::build_pack(&[]);
        body.extend_from_slice(&pack);
        for step in [1usize, 5, 1000] {
            let mut r = Receiver::new(Limits {
                max_pack: 1 << 20,
                max_object: 1 << 20,
            });
            for ch in body.chunks(step) {
                r.feed(ch).unwrap();
            }
            r.finish().unwrap();
            assert_eq!(r.cmds.len(), 2);
            assert!(r.caps.sideband && r.caps.report && r.caps.atomic);
            assert_eq!(r.ingest.as_ref().unwrap().objects(), 0);
        }
        // git's auth probe: a bare flush
        let mut probe = Receiver::new(Limits {
            max_pack: 1,
            max_object: 1,
        });
        probe.feed(b"0000").unwrap();
        probe.finish().unwrap();
        assert!(probe.cmds.is_empty());
    }

    #[test]
    fn directory_file_conflicts() {
        let ix = linear(5);
        let inc = Incoming::new(&[]);
        let mut refs = BTreeMap::new();
        refs.insert("refs/heads/a".to_string(), Oid([5; 20]));
        let cmd = |n: &str| Command {
            old: crate::git::ZERO,
            new: Oid([5; 20]),
            name: n.into(),
        };
        let j = judge(&[cmd("refs/heads/a/b")], &refs, &inc, &ix, |_| false);
        assert!(j[0].1.as_ref().unwrap_err().contains("refs/heads/a"));
        let j = judge(
            &[cmd("refs/heads/x"), cmd("refs/heads/x/y")],
            &BTreeMap::new(),
            &inc,
            &ix,
            |_| false,
        );
        assert!(
            j[0].1.is_err() && j[1].1.is_err(),
            "both sides of a clash in one push"
        );
        let j = judge(&[cmd("refs/heads/ab")], &refs, &inc, &ix, |_| false);
        assert!(j[0].1.is_ok(), "a shared prefix is not a directory");
        // deleting the file makes room for the directory
        let del = Command {
            old: Oid([5; 20]),
            new: crate::git::ZERO,
            name: "refs/heads/a".into(),
        };
        let j = judge(&[del, cmd("refs/heads/a/b")], &refs, &inc, &ix, |_| false);
        assert!(j[0].1.is_ok() && j[1].1.is_ok());
    }

    #[test]
    fn update_rules() {
        let ix = linear(5);
        let inc = Incoming::new(&[]);
        let c = |i: u8| Oid([i; 20]);
        let cmd = |o: Oid, n: Oid, name: &str| Command {
            old: o,
            new: n,
            name: name.into(),
        };
        assert!(check(
            &cmd(c(3), c(5), "refs/heads/main"),
            Some(&c(3)),
            &inc,
            &ix,
            true
        )
        .is_ok());
        assert_eq!(
            check(
                &cmd(c(3), c(5), "refs/heads/main"),
                Some(&c(4)),
                &inc,
                &ix,
                false
            )
            .unwrap_err(),
            "stale info"
        );
        assert!(check(
            &cmd(c(5), c(3), "refs/heads/main"),
            Some(&c(5)),
            &inc,
            &ix,
            true
        )
        .is_err());
        assert!(check(
            &cmd(c(5), c(3), "refs/heads/main"),
            Some(&c(5)),
            &inc,
            &ix,
            false
        )
        .is_ok());
        assert!(check(
            &cmd(c(5), crate::git::ZERO, "refs/heads/main"),
            Some(&c(5)),
            &inc,
            &ix,
            true
        )
        .is_err());
        assert!(check(
            &cmd(crate::git::ZERO, Oid([103; 20]), "refs/heads/t"),
            None,
            &inc,
            &ix,
            false
        )
        .is_err());
        assert!(check(
            &cmd(crate::git::ZERO, Oid([103; 20]), "refs/tags/t"),
            None,
            &inc,
            &ix,
            false
        )
        .is_ok());
        assert!(check(
            &cmd(crate::git::ZERO, c(77), "refs/heads/x"),
            None,
            &inc,
            &ix,
            false
        )
        .is_err());
        assert!(check(
            &cmd(crate::git::ZERO, c(5), "refs/heads/a..b"),
            None,
            &inc,
            &ix,
            false
        )
        .is_err());
    }
}
