//! History walks over the in-memory index: what a fetch must send, what
//! the client already holds (for thin deltas), shallow boundaries, the
//! "ready" test of negotiation, fast-forward and connectivity checks.

use super::repo::{Index, NONE};
use super::Kind;
use std::collections::VecDeque;

pub struct Bits(Vec<u64>);

impl Bits {
    pub fn new(n: usize) -> Bits {
        Bits(vec![0; n / 64 + 1])
    }
    pub fn get(&self, i: u32) -> bool {
        let i = i as usize;
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
    /// true when newly set
    pub fn set(&mut self, i: u32) -> bool {
        let i = i as usize;
        if i / 64 >= self.0.len() {
            self.0.resize(i / 64 + 1, 0);
        }
        let w = &mut self.0[i / 64];
        let m = 1 << (i % 64);
        let new = *w & m == 0;
        *w |= m;
        new
    }
    pub fn clear(&mut self, i: u32) {
        let i = i as usize;
        if let Some(w) = self.0.get_mut(i / 64) {
            *w &= !(1 << (i % 64));
        }
    }
}

fn parents(ix: &Index, c: u32) -> impl Iterator<Item = u32> + '_ {
    ix.kids(c).iter().skip(1).copied()
}

fn tree_of(ix: &Index, c: u32) -> u32 {
    ix.kids(c).first().copied().unwrap_or(NONE)
}

#[derive(Default)]
pub struct Spec {
    pub wants: Vec<u32>,
    /// common objects the client said it has
    pub haves: Vec<u32>,
    /// commits the client holds without their parents
    pub client_shallow: Vec<u32>,
    pub depth: Option<u32>,
    pub deepen_relative: bool,
    pub since: Option<i64>,
    pub not: Vec<u32>,
    pub include_tag: bool,
    /// annotated tags the refs point at (include-tag candidates)
    pub ref_tags: Vec<u32>,
}

pub struct Plan {
    pub send: Vec<u32>,
    /// objects the client is known to hold: legal thin-delta bases
    pub has: Bits,
    pub shallow: Vec<u32>,
    pub unshallow: Vec<u32>,
}

fn check(i: u32) -> Result<u32, String> {
    if i == NONE {
        Err("repository index is missing an object; refusing to send an incomplete pack".into())
    } else {
        Ok(i)
    }
}

/// Mark every tree and blob reachable from tree `t` (stopping at marked ones).
fn mark_tree(ix: &Index, t: u32, bits: &mut Bits) -> Result<(), String> {
    let mut stack = vec![check(t)?];
    if !bits.set(t) {
        return Ok(());
    }
    while let Some(x) = stack.pop() {
        for &k in ix.kids(x) {
            let k = check(k)?;
            if bits.set(k) && ix.obj(k).kind == Kind::Tree {
                stack.push(k);
            }
        }
    }
    Ok(())
}

pub fn plan(ix: &Index, spec: &Spec) -> Result<Plan, String> {
    let n = ix.len();
    let mut wants_c: Vec<u32> = Vec::new();
    let mut send = Vec::new();
    let mut in_send = Bits::new(n);
    let mut other_wants: Vec<u32> = Vec::new();
    for &w in &spec.wants {
        let mut x = check(w)?;
        // tags along a chain are sent, their final target walked below
        while ix.obj(x).kind == Kind::Tag {
            if in_send.set(x) {
                send.push(x);
            }
            x = check(*ix.kids(x).first().ok_or("tag without target")?)?;
        }
        match ix.obj(x).kind {
            Kind::Commit => wants_c.push(x),
            _ => other_wants.push(x),
        }
    }

    // --- shallow boundary ---------------------------------------------------
    let mut graft = Bits::new(n); // commits whose parents the walk must not cross
    let mut cshallow = Bits::new(n);
    for &s in &spec.client_shallow {
        graft.set(s);
        cshallow.set(s);
    }
    let mut shallow_out = Vec::new();
    let mut unshallow = Vec::new();
    let mut not_shallow = Bits::new(n);
    let deepening = spec.depth.is_some() || spec.since.is_some() || !spec.not.is_empty();
    if let Some(depth) = spec.depth {
        let depth = depth.max(1);
        let (heads, depth) = if spec.deepen_relative {
            (spec.client_shallow.clone(), depth.saturating_add(1))
        } else {
            (wants_c.clone(), depth)
        };
        let mut level = vec![0u32; 0];
        level.resize(n, 0);
        let mut q = VecDeque::new();
        for h in heads {
            if level[h as usize] == 0 {
                level[h as usize] = 1;
                q.push_back(h);
            }
        }
        let mut boundary = Vec::new();
        while let Some(c) = q.pop_front() {
            let l = level[c as usize];
            if l >= depth {
                boundary.push(c);
                continue;
            }
            not_shallow.set(c);
            for p in parents(ix, c) {
                let p = check(p)?;
                if level[p as usize] == 0 {
                    level[p as usize] = l + 1;
                    q.push_back(p);
                }
            }
        }
        for c in boundary {
            if !not_shallow.get(c) {
                graft.set(c);
                if !cshallow.get(c) {
                    shallow_out.push(c);
                }
            }
        }
    } else if deepening {
        // deepen-since / deepen-not: the commits reachable from the wants that
        // are new enough and not reachable from the excluded refs
        let mut excluded = Bits::new(n);
        let mut st: Vec<u32> = spec.not.clone();
        while let Some(c) = st.pop() {
            if excluded.set(c) {
                st.extend(parents(ix, c).filter(|&p| p != NONE));
            }
        }
        let mut inc = Bits::new(n);
        let mut order = Vec::new();
        // (the server's whole history: a client's shallow commit inside the
        // range is unshallowed below, so the walk does not stop there)
        let mut st: Vec<u32> = wants_c.clone();
        while let Some(c) = st.pop() {
            if excluded.get(c) || spec.since.is_some_and(|s| ix.obj(c).time < s) {
                continue;
            }
            if inc.set(c) {
                order.push(c);
                st.extend(parents(ix, c).filter(|&p| p != NONE));
            }
        }
        for &c in &order {
            not_shallow.set(c);
        }
        for &c in &order {
            if parents(ix, c).any(|p| p == NONE || !inc.get(p)) && parents(ix, c).next().is_some() {
                not_shallow.clear(c);
                graft.set(c);
                if !cshallow.get(c) {
                    shallow_out.push(c);
                }
            }
        }
    }
    let mut extra_edges = Vec::new();
    if deepening {
        for &s in &spec.client_shallow {
            if not_shallow.get(s) {
                // the client gets the parents as wants; s itself stays a graft
                // for this walk (the client holds it, not what lies behind it)
                unshallow.push(s);
                extra_edges.push(s);
                for p in parents(ix, s) {
                    wants_c.push(check(p)?);
                }
            }
        }
    }

    // --- what the client has ------------------------------------------------
    let mut has_c = Bits::new(n);
    let mut has = Bits::new(n);
    let mut st: Vec<u32> = Vec::new();
    for &h in &spec.haves {
        let h = check(h)?;
        match ix.obj(h).kind {
            Kind::Commit => st.push(h),
            Kind::Tag => {
                has.set(h);
                if let Some(p) = ix.peel(h) {
                    if ix.obj(p).kind == Kind::Commit {
                        st.push(p);
                    } else {
                        has.set(p);
                    }
                }
            }
            _ => {
                has.set(h);
            }
        }
    }
    let have_heads: Vec<u32> = st.clone();
    // the client's history ends at its own shallow commits
    let mut client_graft = Bits::new(n);
    for &s in &spec.client_shallow {
        client_graft.set(s);
    }
    while let Some(c) = st.pop() {
        if has_c.set(c) && !client_graft.get(c) {
            st.extend(parents(ix, c).filter(|&p| p != NONE));
        }
    }

    // --- the commits to send --------------------------------------------------
    let mut edges = have_heads;
    edges.extend(extra_edges);
    let mut seen = Bits::new(n);
    let mut commits = Vec::new();
    let mut st: Vec<u32> = wants_c.clone();
    while let Some(c) = st.pop() {
        if has_c.get(c) {
            edges.push(c);
            continue;
        }
        if !seen.set(c) {
            continue;
        }
        commits.push(c);
        if !graft.get(c) {
            for p in parents(ix, c) {
                st.push(check(p)?);
            }
        }
    }
    // trees the client holds through the edge commits
    for &e in &edges {
        has.set(e);
        let t = tree_of(ix, e);
        if t != NONE {
            mark_tree(ix, t, &mut has)?;
        }
    }
    // newest first, as git orders them
    commits.sort_by_key(|&c| std::cmp::Reverse(ix.obj(c).time));
    for &c in &commits {
        if in_send.set(c) {
            send.push(c);
        }
    }
    for &c in &commits {
        let t = check(tree_of(ix, c))?;
        let mut stack = vec![t];
        while let Some(x) = stack.pop() {
            if has.get(x) || !in_send.set(x) {
                continue;
            }
            send.push(x);
            if ix.obj(x).kind == Kind::Tree {
                for &k in ix.kids(x) {
                    stack.push(check(k)?);
                }
            }
        }
    }
    for x in other_wants {
        let mut stack = vec![x];
        while let Some(y) = stack.pop() {
            if has.get(y) || !in_send.set(y) {
                continue;
            }
            send.push(y);
            if ix.obj(y).kind == Kind::Tree {
                for &k in ix.kids(y) {
                    stack.push(check(k)?);
                }
            }
        }
    }
    if spec.include_tag {
        for &t in &spec.ref_tags {
            if in_send.get(t) || has.get(t) {
                continue;
            }
            // the tag chain's final target must be in the pack
            let mut chain = vec![t];
            let mut x = t;
            let mut ok = false;
            for _ in 0..64 {
                let Some(&k) = ix.kids(x).first() else { break };
                if k == NONE {
                    break;
                }
                if ix.obj(k).kind == Kind::Tag {
                    chain.push(k);
                    x = k;
                    continue;
                }
                ok = in_send.get(k);
                break;
            }
            if ok {
                for c in chain {
                    if in_send.set(c) {
                        send.push(c);
                    }
                }
            }
        }
    }
    Ok(Plan {
        send,
        has,
        shallow: shallow_out,
        unshallow,
    })
}

/// Every object reachable from `tips` (the refs): exactly what a reader may
/// fetch. Unreachable objects (a force-pushed-away commit, a deleted branch)
/// stay stored until a repack drops them, and must not be served.
pub fn closure(ix: &Index, tips: &[u32]) -> Bits {
    let mut b = Bits::new(ix.len());
    let mut st: Vec<u32> = tips.iter().copied().filter(|&t| t != NONE).collect();
    while let Some(x) = st.pop() {
        if !b.set(x) {
            continue;
        }
        st.extend(ix.kids(x).iter().copied().filter(|&k| k != NONE));
    }
    b
}

/// Negotiation's "ready": every wanted commit reaches a common commit (or a
/// parent of one), so a pack can be cut. Memoized over the whole walk.
pub fn ready(ix: &Index, wants: &[u32], common: &[u32]) -> bool {
    if common.is_empty() {
        return false;
    }
    let n = ix.len();
    let mut target = Bits::new(n);
    let mut oldest = i64::MAX;
    for &h in common {
        let h = match ix.peel(h) {
            Some(x) if ix.obj(x).kind == Kind::Commit => x,
            _ => continue,
        };
        target.set(h);
        oldest = oldest.min(ix.obj(h).time);
        for p in parents(ix, h) {
            if p != NONE {
                target.set(p);
            }
        }
    }
    if oldest == i64::MAX {
        return false;
    }
    // 0 = unknown, 1 = in progress / no, 2 = reaches
    let mut state = vec![0u8; n];
    for &w in wants {
        let w = match ix.peel(w) {
            Some(x) if ix.obj(x).kind == Kind::Commit => x,
            _ => continue, // non-commit wants never block readiness
        };
        let mut stack: Vec<(u32, bool)> = vec![(w, false)];
        while let Some((c, expanded)) = stack.pop() {
            if expanded {
                let r =
                    target.get(c) || parents(ix, c).any(|p| p != NONE && state[p as usize] == 2);
                state[c as usize] = if r { 2 } else { 1 };
                continue;
            }
            if state[c as usize] != 0 {
                continue;
            }
            if target.get(c) {
                state[c as usize] = 2;
                continue;
            }
            state[c as usize] = 1;
            // commits older than every common commit cannot lead to one
            if ix.obj(c).time < oldest {
                continue;
            }
            stack.push((c, true));
            for p in parents(ix, c) {
                if p != NONE && state[p as usize] == 0 {
                    stack.push((p, false));
                }
            }
        }
        if state[w as usize] != 2 {
            return false;
        }
    }
    true
}

/// Is commit `old` an ancestor of (or equal to) commit `new`?
pub fn is_ancestor(ix: &Index, old: u32, new: u32) -> bool {
    if old == new {
        return true;
    }
    let mut seen = Bits::new(ix.len());
    let mut st = vec![new];
    while let Some(c) = st.pop() {
        if c == old {
            return true;
        }
        if c == NONE || !seen.set(c) || ix.obj(c).kind != Kind::Commit {
            continue;
        }
        st.extend(parents(ix, c));
    }
    false
}

/// Is `x` reachable from any of `tips`?
#[cfg(test)]
pub fn reachable(ix: &Index, tips: &[u32], x: u32) -> bool {
    let target_commit = ix.obj(x).kind == Kind::Commit;
    let mut seen = Bits::new(ix.len());
    let mut st: Vec<u32> = tips.to_vec();
    while let Some(c) = st.pop() {
        if c == x {
            return true;
        }
        if c == NONE || !seen.set(c) {
            continue;
        }
        match ix.obj(c).kind {
            Kind::Commit => {
                if target_commit {
                    st.extend(parents(ix, c));
                } else {
                    st.extend(ix.kids(c).iter().copied());
                }
            }
            Kind::Tag | Kind::Tree => st.extend(ix.kids(c).iter().copied()),
            Kind::Blob => {}
        }
    }
    false
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::git::ingest::IdxEntry;
    use crate::git::repo::PackInfo;
    use crate::git::Oid;
    use std::rc::Rc;

    /// A linear history c1 <- c2 <- ... <- cN, commit i has tree ti with blob bi
    /// plus a blob shared by every tree.
    pub fn linear(n: u8) -> Index {
        let shared = Oid([250; 20]);
        let mut es = vec![IdxEntry {
            oid: shared,
            offset: 0,
            len: 1,
            stored: 3,
            base: None,
            kind: Kind::Blob,
            size: 1,
            time: 0,
            children: vec![],
        }];
        for i in 1..=n {
            let (c, t, b) = (Oid([i; 20]), Oid([100 + i; 20]), Oid([200 - i; 20]));
            let mut kids = vec![(Kind::Tree, t)];
            if i > 1 {
                kids.push((Kind::Commit, Oid([i - 1; 20])));
            }
            for (oid, kind, children, time) in [
                (b, Kind::Blob, vec![], 0),
                (
                    t,
                    Kind::Tree,
                    vec![(Kind::Blob, b), (Kind::Blob, shared)],
                    0,
                ),
                (c, Kind::Commit, kids, 1000 + i as i64),
            ] {
                es.push(IdxEntry {
                    oid,
                    offset: es.len() as u64,
                    len: 1,
                    stored: kind as u8,
                    base: None,
                    kind,
                    size: 1,
                    time,
                    children,
                });
            }
        }
        let mut ix = Index::default();
        ix.add_pack(
            Rc::new(PackInfo {
                id: "t".into(),
                len: 1,
                chunk: 262144,
            }),
            &es,
        )
        .unwrap();
        ix
    }

    fn c(ix: &Index, i: u8) -> u32 {
        ix.lookup(&Oid([i; 20])).unwrap()
    }

    #[test]
    fn clone_sends_everything() {
        let ix = linear(5);
        let p = plan(
            &ix,
            &Spec {
                wants: vec![c(&ix, 5)],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.send.len(), ix.len());
        // newest commit first
        assert_eq!(p.send[0], c(&ix, 5));
    }

    #[test]
    fn incremental_fetch_sends_only_new() {
        let ix = linear(5);
        let p = plan(
            &ix,
            &Spec {
                wants: vec![c(&ix, 5)],
                haves: vec![c(&ix, 3)],
                ..Default::default()
            },
        )
        .unwrap();
        // c4, c5, their trees and their blobs; the shared blob is had
        assert_eq!(p.send.len(), 6);
        assert!(p.has.get(ix.lookup(&Oid([250; 20])).unwrap()));
        assert!(p.has.get(ix.lookup(&Oid([103; 20])).unwrap()));
        assert!(!p.has.get(c(&ix, 4)));
        assert!(ready(&ix, &[c(&ix, 5)], &[c(&ix, 3)]));
        // a want behind a common commit is already held (git marks the parents of haves too)
        assert!(ready(&ix, &[c(&ix, 2)], &[c(&ix, 3)]));
        assert!(!ready(&ix, &[c(&ix, 5)], &[]));
    }

    #[test]
    fn shallow_depth_and_deepen() {
        let ix = linear(5);
        let p = plan(
            &ix,
            &Spec {
                wants: vec![c(&ix, 5)],
                depth: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.shallow, vec![c(&ix, 4)]);
        assert_eq!(p.send.len(), 2 + 2 * 2 + 1);
        // deepen by one more from that shallow clone
        let p2 = plan(
            &ix,
            &Spec {
                wants: vec![c(&ix, 5)],
                haves: vec![c(&ix, 5)],
                client_shallow: vec![c(&ix, 4)],
                depth: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p2.shallow, vec![c(&ix, 3)]);
        assert_eq!(p2.unshallow, vec![c(&ix, 4)]);
        // c3, its tree and its blob
        assert_eq!(p2.send.len(), 3);
        let p3 = plan(
            &ix,
            &Spec {
                wants: vec![c(&ix, 5)],
                since: Some(1004),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p3.shallow, vec![c(&ix, 4)]);
    }

    #[test]
    fn ancestry() {
        let ix = linear(5);
        assert!(is_ancestor(&ix, c(&ix, 2), c(&ix, 5)));
        assert!(!is_ancestor(&ix, c(&ix, 5), c(&ix, 2)));
        assert!(reachable(&ix, &[c(&ix, 5)], c(&ix, 1)));
        assert!(reachable(
            &ix,
            &[c(&ix, 5)],
            ix.lookup(&Oid([199; 20])).unwrap()
        ));
        assert!(!reachable(&ix, &[c(&ix, 2)], c(&ix, 4)));
    }
}
