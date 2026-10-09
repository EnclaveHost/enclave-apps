//! upload-pack: ref advertisement and fetch, in protocol v2 (ls-refs,
//! fetch) and v0/v1 (the stateless-RPC negotiation with multi_ack_detailed
//! and no-done that libgit2, go-git and older git speak over HTTP).

use super::packgen::{Gen, PackIo};
use super::pkt::{self, Pkt, Reader};
use super::repo::Index;
use super::walk::{self, Spec};
use super::{Kind, Oid};
use std::collections::BTreeMap;

pub const AGENT: &str = concat!("depot/", env!("CARGO_PKG_VERSION"));

/// What the protocol needs of a repository: its index, refs and HEAD.
pub struct View<'a> {
    pub ix: &'a Index,
    pub refs: &'a BTreeMap<String, Oid>,
    /// HEAD's symref target, e.g. refs/heads/main
    pub head: &'a str,
}

impl View<'_> {
    fn head_oid(&self) -> Option<Oid> {
        self.refs.get(self.head).copied()
    }
    fn peeled(&self, oid: &Oid) -> Option<Oid> {
        let i = self.ix.lookup(oid)?;
        if self.ix.obj(i).kind != Kind::Tag {
            return None;
        }
        self.ix.peel(i).map(|p| self.ix.oid(p))
    }
    fn tip_indices(&self) -> Vec<u32> {
        self.refs
            .values()
            .filter_map(|o| self.ix.lookup(o))
            .collect()
    }
    fn ref_tags(&self) -> Vec<u32> {
        self.refs
            .values()
            .filter_map(|o| self.ix.lookup(o))
            .filter(|&i| self.ix.obj(i).kind == Kind::Tag)
            .collect()
    }
    /// Wants are refused unless they are reachable from a ref: a deleted
    /// branch's objects stay unfetchable.
    fn check_want(&self, oid: &Oid, tips: &mut Option<Vec<u32>>) -> Result<u32, String> {
        let i = self
            .ix
            .lookup(oid)
            .ok_or_else(|| format!("upload-pack: not our ref {oid}"))?;
        if self.refs.values().any(|r| r == oid) {
            return Ok(i);
        }
        let t = tips.get_or_insert_with(|| self.tip_indices());
        if walk::reachable(self.ix, t, i) {
            Ok(i)
        } else {
            Err(format!("upload-pack: not our ref {oid}"))
        }
    }
    fn resolve_ref(&self, name: &str) -> Option<Oid> {
        for cand in [
            name.to_string(),
            format!("refs/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/tags/{name}"),
        ] {
            if let Some(o) = self.refs.get(&cand) {
                return Some(*o);
            }
        }
        Oid::from_hex(name)
    }
}

pub fn error_pkt(msg: &str) -> Vec<u8> {
    let mut b = Vec::new();
    let m: String = msg.chars().take(900).collect();
    pkt::line(&mut b, &format!("ERR {m}\n"));
    b
}

/// The stream that answers a fetch: negotiation lines, then the pack in
/// side-band packets (or raw), progress on band 2, then a flush.
pub struct Fetch {
    pub head: Vec<u8>,
    pub gen: Option<Gen>,
    /// side-band payload ceiling, None for a raw pack (v0 without side-band)
    pub band: Option<usize>,
    pub progress: bool,
    pub trailer_flush: bool,
    started: bool,
    finished: bool,
    last_pct: i64,
}

impl Fetch {
    fn lines_only(head: Vec<u8>) -> Fetch {
        Fetch {
            head,
            gen: None,
            band: None,
            progress: false,
            trailer_flush: false,
            started: false,
            finished: false,
            last_pct: -1,
        }
    }

    pub fn objects(&self) -> usize {
        self.gen.as_ref().map(|g| g.total()).unwrap_or(0)
    }

    /// Append the next part of the response; Ok(true) when complete.
    pub fn produce(
        &mut self,
        io: &mut dyn PackIo,
        out: &mut Vec<u8>,
        budget: usize,
    ) -> Result<bool, String> {
        if !self.started {
            self.started = true;
            out.append(&mut self.head);
            if let (Some(g), Some(max), true) = (&self.gen, self.band, self.progress) {
                let msg = format!("Enumerating objects: {}, done.\n", g.total());
                pkt::band(out, 2, msg.as_bytes(), max);
            }
            return Ok(self.gen.is_none() && !self.trailer_flush);
        }
        if self.finished {
            return Ok(true);
        }
        let Some(g) = self.gen.as_mut() else {
            if self.trailer_flush {
                pkt::flush(out);
            }
            self.finished = true;
            return Ok(true);
        };
        let mut raw = Vec::with_capacity(budget + 1024);
        let done = g.produce(io, &mut raw, budget)?;
        match self.band {
            Some(max) => {
                pkt::band(out, 1, &raw, max);
                if self.progress {
                    let total = g.total().max(1);
                    let pct = (g.sent() * 100 / total) as i64;
                    if pct != self.last_pct || done {
                        self.last_pct = pct;
                        let end = if done { ", done.\n" } else { "\r" };
                        let msg = format!(
                            "Sending objects: {:3}% ({}/{}){}",
                            pct,
                            g.sent(),
                            g.total(),
                            end
                        );
                        pkt::band(out, 2, msg.as_bytes(), max);
                    }
                }
                if done {
                    pkt::flush(out);
                }
            }
            None => out.extend_from_slice(&raw),
        }
        if done {
            self.finished = true;
        }
        Ok(done)
    }
}

// ---- protocol v2 ------------------------------------------------------------

pub fn advertise_v2() -> Vec<u8> {
    let mut b = Vec::new();
    for l in [
        "version 2\n".to_string(),
        format!("agent={AGENT}\n"),
        "ls-refs=unborn\n".into(),
        "fetch=shallow\n".into(),
        "server-option\n".into(),
        "object-format=sha1\n".into(),
    ] {
        pkt::line(&mut b, &l);
    }
    pkt::flush(&mut b);
    b
}

/// A v2 request: `command=<c>`, capability lines, a delimiter, arguments.
pub struct Request {
    pub command: String,
    pub args: Vec<String>,
}

pub fn parse_v2(body: &[u8]) -> Result<Request, String> {
    let mut r = Reader::new(body);
    let mut command = String::new();
    let mut args = Vec::new();
    let mut in_args = false;
    loop {
        match r.next()? {
            None => return Err("truncated v2 request".into()),
            Some(Pkt::Flush) => break,
            Some(Pkt::Delim) => in_args = true,
            Some(Pkt::End) => return Err("unexpected response-end".into()),
            Some(p) => {
                let t = p.text().ok_or("non-text v2 request line")?;
                if in_args {
                    args.push(t.to_string());
                } else if let Some(c) = t.strip_prefix("command=") {
                    command = c.to_string();
                } else if let Some(f) = t.strip_prefix("object-format=") {
                    if f != "sha1" {
                        return Err(format!("object-format {f} is not served"));
                    }
                }
            }
        }
    }
    if command.is_empty() {
        return Err("v2 request names no command".into());
    }
    Ok(Request { command, args })
}

pub fn ls_refs(v: &View, args: &[String]) -> Vec<u8> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let peel = args.iter().any(|a| a == "peel");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let mut b = Vec::new();
    if wanted("HEAD") {
        match v.head_oid() {
            Some(o) => {
                let mut l = format!("{o} HEAD");
                if symrefs {
                    l.push_str(&format!(" symref-target:{}", v.head));
                }
                l.push('\n');
                pkt::line(&mut b, &l);
            }
            None if unborn && symrefs => {
                pkt::line(&mut b, &format!("unborn HEAD symref-target:{}\n", v.head))
            }
            None => {}
        }
    }
    for (name, o) in v.refs {
        if !wanted(name) {
            continue;
        }
        let mut l = format!("{o} {name}");
        if peel {
            if let Some(p) = v.peeled(o) {
                l.push_str(&format!(" peeled:{p}"));
            }
        }
        l.push('\n');
        pkt::line(&mut b, &l);
    }
    pkt::flush(&mut b);
    b
}

fn oid_arg(s: &str) -> Result<Oid, String> {
    Oid::from_hex(s.split(' ').next().unwrap_or("")).ok_or_else(|| format!("bad object id: {s}"))
}

pub fn fetch_v2(v: &View, args: &[String]) -> Result<Fetch, String> {
    let ix = v.ix;
    let mut spec = Spec::default();
    let mut tips = None;
    let (mut done, mut thin, mut ofs, mut progress, mut seen_haves) =
        (false, false, false, true, false);
    let mut shallow_requested = false;
    let mut common: Vec<u32> = Vec::new();
    for a in args {
        let (k, val) = a.split_once(' ').unwrap_or((a.as_str(), ""));
        match k {
            "want" => {
                let o = oid_arg(val)?;
                spec.wants.push(v.check_want(&o, &mut tips)?);
            }
            "have" => {
                seen_haves = true;
                if let Some(i) = ix.lookup(&oid_arg(val)?) {
                    if !common.contains(&i) {
                        common.push(i);
                    }
                }
            }
            "done" => done = true,
            "thin-pack" => thin = true,
            "ofs-delta" => ofs = true,
            "no-progress" => progress = false,
            "include-tag" => spec.include_tag = true,
            "shallow" => {
                shallow_requested = true;
                if let Some(i) = ix.lookup(&oid_arg(val)?) {
                    spec.client_shallow.push(i);
                }
            }
            "deepen" => {
                shallow_requested = true;
                let d: u32 = val.trim().parse().map_err(|_| "bad deepen")?;
                spec.depth = Some(d);
            }
            "deepen-relative" => spec.deepen_relative = true,
            "deepen-since" => {
                shallow_requested = true;
                spec.since = Some(val.trim().parse().map_err(|_| "bad deepen-since")?);
            }
            "deepen-not" => {
                shallow_requested = true;
                let o = v
                    .resolve_ref(val.trim())
                    .ok_or_else(|| format!("deepen-not: no such ref {val}"))?;
                if let Some(i) = ix.lookup(&o) {
                    spec.not.push(i);
                }
            }
            "filter" => {
                return Err(
                    "object filters (partial clone) are not supported by this server".into(),
                )
            }
            "want-ref" => return Err("want-ref is not supported by this server".into()),
            "sideband-all" | "wait-for-done" => {}
            k if k.starts_with("packfile-uris") => {}
            _ => {}
        }
    }
    if spec.depth.is_some() && (spec.since.is_some() || !spec.not.is_empty()) {
        return Err("deepen and deepen-since (or deepen-not) cannot be used together".into());
    }
    let mut head = Vec::new();
    if spec.wants.is_empty() {
        pkt::flush(&mut head);
        return Ok(Fetch::lines_only(head));
    }
    if seen_haves && !done {
        pkt::line(&mut head, "acknowledgments\n");
        if common.is_empty() {
            pkt::line(&mut head, "NAK\n");
        }
        for &c in &common {
            pkt::line(&mut head, &format!("ACK {}\n", ix.oid(c)));
        }
        if walk::ready(ix, &spec.wants, &common) {
            pkt::line(&mut head, "ready\n");
            pkt::delim(&mut head);
        } else {
            pkt::flush(&mut head);
            return Ok(Fetch::lines_only(head));
        }
    }
    spec.haves = common;
    spec.ref_tags = v.ref_tags();
    let plan = walk::plan(ix, &spec)?;
    if shallow_requested {
        pkt::line(&mut head, "shallow-info\n");
        for &s in &plan.shallow {
            pkt::line(&mut head, &format!("shallow {}\n", ix.oid(s)));
        }
        for &s in &plan.unshallow {
            pkt::line(&mut head, &format!("unshallow {}\n", ix.oid(s)));
        }
        pkt::delim(&mut head);
    }
    pkt::line(&mut head, "packfile\n");
    let gen = Gen::new(ix, plan, ofs, thin);
    Ok(Fetch {
        head,
        gen: Some(gen),
        band: Some(pkt::MAX_BAND),
        progress,
        trailer_flush: true,
        started: false,
        finished: false,
        last_pct: -1,
    })
}

// ---- protocol v0 ------------------------------------------------------------

const V0_CAPS: &str = "multi_ack thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative no-progress include-tag multi_ack_detailed allow-tip-sha1-in-want allow-reachable-sha1-in-want no-done object-format=sha1";

/// Ref lines (`<oid> <name>`, with `^{}` peeled lines when `peel`), the
/// first carrying the capabilities after a NUL.
pub fn advertise_refs(v: &View, caps: &str, include_head: bool, peel: bool) -> Vec<u8> {
    let mut lines: Vec<(Oid, String)> = Vec::new();
    if include_head {
        if let Some(o) = v.head_oid() {
            lines.push((o, "HEAD".into()));
        }
    }
    for (name, o) in v.refs {
        lines.push((*o, name.clone()));
        if peel {
            if let Some(p) = v.peeled(o) {
                lines.push((p, format!("{name}^{{}}")));
            }
        }
    }
    let mut b = Vec::new();
    if lines.is_empty() {
        pkt::line(
            &mut b,
            &format!("{} capabilities^{{}}\0{caps}\n", super::ZERO),
        );
    }
    for (i, (o, n)) in lines.iter().enumerate() {
        if i == 0 {
            pkt::line(&mut b, &format!("{o} {n}\0{caps}\n"));
        } else {
            pkt::line(&mut b, &format!("{o} {n}\n"));
        }
    }
    pkt::flush(&mut b);
    b
}

pub fn advertise_v0_upload(v: &View) -> Vec<u8> {
    let mut caps = V0_CAPS.to_string();
    if v.head_oid().is_some() {
        caps.push_str(&format!(" symref=HEAD:{}", v.head));
    }
    caps.push_str(&format!(" agent={AGENT}"));
    advertise_refs(v, &caps, true, true)
}

pub fn upload_v0(v: &View, body: &[u8]) -> Result<Fetch, String> {
    let ix = v.ix;
    let mut r = Reader::new(body);
    let mut spec = Spec::default();
    let mut caps: Vec<String> = Vec::new();
    let mut tips = None;
    let mut first = true;
    // the want section
    loop {
        match r.next()? {
            None => return Err("truncated upload-pack request".into()),
            Some(Pkt::Flush) => break,
            Some(p) => {
                let t = p.text().ok_or("bad request line")?;
                if let Some(rest) = t.strip_prefix("want ") {
                    let (o, c) = rest.split_once(' ').unwrap_or((rest, ""));
                    if first {
                        caps = c
                            .split(' ')
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect();
                        first = false;
                    }
                    let i = v.check_want(&oid_arg(o)?, &mut tips)?;
                    if !spec.wants.contains(&i) {
                        spec.wants.push(i);
                    }
                } else if let Some(o) = t.strip_prefix("shallow ") {
                    if let Some(i) = ix.lookup(&oid_arg(o)?) {
                        spec.client_shallow.push(i);
                    }
                } else if let Some(d) = t.strip_prefix("deepen-since ") {
                    spec.since = Some(d.trim().parse().map_err(|_| "bad deepen-since")?);
                } else if let Some(n) = t.strip_prefix("deepen-not ") {
                    let o = v
                        .resolve_ref(n.trim())
                        .ok_or_else(|| format!("deepen-not: no such ref {n}"))?;
                    if let Some(i) = ix.lookup(&o) {
                        spec.not.push(i);
                    }
                } else if let Some(d) = t.strip_prefix("deepen ") {
                    spec.depth = Some(d.trim().parse().map_err(|_| "bad deepen")?);
                } else if t.starts_with("filter ") {
                    return Err(
                        "object filters (partial clone) are not supported by this server".into(),
                    );
                } else {
                    return Err(format!("unexpected line in want section: {t}"));
                }
            }
        }
    }
    let has = |c: &str| caps.iter().any(|x| x == c);
    let detailed = has("multi_ack_detailed");
    let multi = detailed || has("multi_ack");
    let no_done = has("no-done");
    let band = if has("side-band-64k") {
        Some(pkt::MAX_BAND)
    } else if has("side-band") {
        Some(995)
    } else {
        None
    };
    spec.include_tag = has("include-tag");
    spec.deepen_relative = has("deepen-relative");
    let progress = !has("no-progress") && band.is_some();
    let thin = has("thin-pack");
    let ofs = has("ofs-delta");
    if spec.wants.is_empty() {
        return Ok(Fetch::lines_only(Vec::new()));
    }
    if spec.depth.is_some() && (spec.since.is_some() || !spec.not.is_empty()) {
        return Err("deepen and deepen-since (or deepen-not) cannot be used together".into());
    }
    let deepen = spec.depth.is_some() || spec.since.is_some() || !spec.not.is_empty();
    let mut head = Vec::new();
    spec.ref_tags = v.ref_tags();
    if deepen {
        let p = walk::plan(ix, &spec)?;
        for &s in &p.shallow {
            pkt::line(&mut head, &format!("shallow {}\n", ix.oid(s)));
        }
        for &s in &p.unshallow {
            pkt::line(&mut head, &format!("unshallow {}\n", ix.oid(s)));
        }
        pkt::flush(&mut head);
    }
    // negotiation
    let mut common: Vec<u32> = Vec::new();
    let mut last: Option<Oid> = None;
    let (mut got_common, mut got_other, mut sent_ready) = (false, false, false);
    let mut ready_cache: Option<(usize, bool)> = None;
    let mut ready = |common: &Vec<u32>| -> bool {
        if let Some((n, r)) = ready_cache {
            if n == common.len() {
                return r;
            }
        }
        let r = walk::ready(ix, &spec.wants, common);
        ready_cache = Some((common.len(), r));
        r
    };
    let send_pack;
    loop {
        match r.next()? {
            None => {
                if deepen && common.is_empty() && !got_other {
                    // the deepen-only first request of a shallow fetch
                    return Ok(Fetch::lines_only(head));
                }
                return Err("upload-pack request ended without flush or done".into());
            }
            Some(Pkt::Flush) => {
                if detailed && got_common && !got_other && ready(&common) {
                    sent_ready = true;
                    pkt::line(&mut head, &format!("ACK {} ready\n", last.unwrap()));
                }
                if common.is_empty() || multi {
                    pkt::line(&mut head, "NAK\n");
                }
                if no_done && sent_ready {
                    pkt::line(&mut head, &format!("ACK {}\n", last.unwrap()));
                    send_pack = true;
                } else {
                    send_pack = false;
                }
                break;
            }
            Some(p) => {
                let t = p.text().ok_or("bad request line")?;
                if let Some(o) = t.strip_prefix("have ") {
                    let o = oid_arg(o)?;
                    match ix.lookup(&o) {
                        Some(i) => {
                            got_common = true;
                            if !common.contains(&i) {
                                common.push(i);
                            }
                            last = Some(o);
                            if detailed {
                                pkt::line(&mut head, &format!("ACK {o} common\n"));
                            } else if multi {
                                pkt::line(&mut head, &format!("ACK {o} continue\n"));
                            } else if common.len() == 1 {
                                pkt::line(&mut head, &format!("ACK {o}\n"));
                            }
                        }
                        None => {
                            got_other = true;
                            if multi && ready(&common) {
                                if detailed {
                                    sent_ready = true;
                                    pkt::line(&mut head, &format!("ACK {o} ready\n"));
                                } else {
                                    pkt::line(&mut head, &format!("ACK {o} continue\n"));
                                }
                            }
                        }
                    }
                } else if t == "done" {
                    match last {
                        Some(l) if !common.is_empty() => {
                            if multi {
                                pkt::line(&mut head, &format!("ACK {l}\n"));
                            }
                        }
                        _ => pkt::line(&mut head, "NAK\n"),
                    }
                    send_pack = true;
                    break;
                } else {
                    return Err(format!("expected have/done, got {t}"));
                }
            }
        }
    }
    if !send_pack {
        return Ok(Fetch::lines_only(head));
    }
    spec.haves = common;
    let plan = walk::plan(ix, &spec)?;
    let gen = Gen::new(ix, plan, ofs, thin);
    Ok(Fetch {
        head,
        gen: Some(gen),
        band,
        progress,
        trailer_flush: false,
        started: false,
        finished: false,
        last_pct: -1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::walk::tests::linear;

    fn view(ix: &Index) -> (BTreeMap<String, Oid>, String) {
        let _ = ix;
        let mut refs = BTreeMap::new();
        refs.insert("refs/heads/main".to_string(), Oid([5; 20]));
        (refs, "refs/heads/main".into())
    }

    fn text_lines(b: &[u8]) -> Vec<String> {
        let mut r = Reader::new(b);
        let mut out = Vec::new();
        while let Ok(Some(p)) = r.next() {
            out.push(match p {
                Pkt::Flush => "0000".into(),
                Pkt::Delim => "0001".into(),
                Pkt::End => "0002".into(),
                p => p.text().unwrap_or("<bin>").replace('\0', " NUL "),
            });
        }
        out
    }

    #[test]
    fn v2_ls_refs_and_negotiation() {
        let ix = linear(5);
        let (refs, head) = view(&ix);
        let v = View {
            ix: &ix,
            refs: &refs,
            head: &head,
        };
        let l = text_lines(&ls_refs(
            &v,
            &["symrefs".into(), "peel".into(), "ref-prefix HEAD".into()],
        ));
        assert_eq!(
            l,
            vec![
                format!("{} HEAD symref-target:refs/heads/main", Oid([5; 20])),
                "0000".into()
            ]
        );
        // a have we know but cannot be ready on: acks and stops
        let f = fetch_v2(
            &v,
            &[
                format!("want {}", Oid([5; 20])),
                format!("have {}", Oid([77; 20])),
            ],
        )
        .unwrap();
        assert!(f.gen.is_none());
        assert_eq!(text_lines(&f.head), vec!["acknowledgments", "NAK", "0000"]);
        let f = fetch_v2(
            &v,
            &[
                format!("want {}", Oid([5; 20])),
                format!("have {}", Oid([3; 20])),
            ],
        )
        .unwrap();
        assert_eq!(
            text_lines(&f.head)[..3],
            [
                "acknowledgments".to_string(),
                format!("ACK {}", Oid([3; 20])),
                "ready".into()
            ]
        );
        assert_eq!(f.objects(), 6);
        assert!(fetch_v2(&v, &[format!("want {}", Oid([99; 20]))]).is_err());
        // a non-tip but reachable commit is fetchable
        assert!(fetch_v2(&v, &[format!("want {}", Oid([2; 20])), "done".into()]).is_ok());
    }

    #[test]
    fn v0_stateless_rounds() {
        let ix = linear(5);
        let (refs, head) = view(&ix);
        let v = View {
            ix: &ix,
            refs: &refs,
            head: &head,
        };
        let adv = text_lines(&advertise_v0_upload(&v));
        assert!(adv[0].contains("HEAD NUL multi_ack"));
        assert!(adv[0].contains("symref=HEAD:refs/heads/main"));
        let mut body = Vec::new();
        pkt::line(
            &mut body,
            &format!(
                "want {} multi_ack_detailed no-done side-band-64k thin-pack ofs-delta\n",
                Oid([5; 20])
            ),
        );
        pkt::flush(&mut body);
        // a round with an unknown have first: nothing to be ready on yet
        let mut round = body.clone();
        pkt::line(&mut round, &format!("have {}\n", Oid([9; 20])));
        pkt::line(&mut round, &format!("have {}\n", Oid([4; 20])));
        pkt::flush(&mut round);
        let f = upload_v0(&v, &round).unwrap();
        assert_eq!(
            text_lines(&f.head),
            vec![format!("ACK {} common", Oid([4; 20])), "NAK".into()]
        );
        assert!(f.gen.is_none());
        // common first: the unknown have after it is answered ready, and with
        // no-done the pack follows in the same response
        let mut round = body.clone();
        pkt::line(&mut round, &format!("have {}\n", Oid([4; 20])));
        pkt::line(&mut round, &format!("have {}\n", Oid([9; 20])));
        pkt::flush(&mut round);
        let f = upload_v0(&v, &round).unwrap();
        assert_eq!(
            text_lines(&f.head),
            vec![
                format!("ACK {} common", Oid([4; 20])),
                format!("ACK {} ready", Oid([9; 20])),
                "NAK".into(),
                format!("ACK {}", Oid([4; 20])),
            ]
        );
        assert_eq!(f.objects(), 3);
        let mut done = body.clone();
        pkt::line(&mut done, "done\n");
        let f = upload_v0(&v, &done).unwrap();
        assert_eq!(text_lines(&f.head), vec!["NAK"]);
        assert_eq!(f.objects(), ix.len());
    }
}
