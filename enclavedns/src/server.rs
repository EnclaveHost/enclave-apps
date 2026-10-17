//! Turning a DNS query into a response, whatever it arrived over.
//!
//! Each hop of a CNAME chain goes to whichever source owns the name: a
//! hosted zone, ENS for `.eth`, the onion validator for `.onion`, or the
//! validating resolver for the rest. That is why chains are followed here
//! and not in the resolver: a public name that CNAMEs into `.onion` must
//! not send the onion name to the internet.

use crate::auth::Zone;
use crate::ens::Ens;
use crate::name::Name;
use crate::onion::{self, OnionConfig};
use crate::resolver::{Answer, Ctx, Resolver, Sec};
use crate::wire::{ede, rcode, ty, Edns, Message, Record, CLASS_IN, OPT_PADDING};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    Udp,
    Tcp,
    Tls,
    Https,
}

#[derive(Clone, Debug)]
pub struct Resolution {
    pub rcode: u16,
    pub answer: Vec<Record>,
    pub authority: Vec<Record>,
    pub additional: Vec<Record>,
    pub sec: Sec,
    pub ede: Option<(u16, String)>,
    pub aa: bool,
    pub sources: Vec<&'static str>,
}

#[derive(Default, Clone, Debug)]
pub struct Counters {
    pub udp: u64,
    pub tcp: u64,
    pub tls: u64,
    pub https: u64,
    pub noerror: u64,
    pub nxdomain: u64,
    pub servfail: u64,
    pub refused: u64,
    pub ad: u64,
    pub rate_limited: u64,
    pub ens: u64,
    pub onion: u64,
    pub zone: u64,
    pub latency_ms_total: u64,
    pub answered: u64,
}

struct Slot {
    result: RefCell<Option<Resolution>>,
    wakers: RefCell<Vec<Waker>>,
}

struct WaitSlot(Rc<Slot>);

impl Future for WaitSlot {
    type Output = Resolution;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Resolution> {
        if let Some(r) = self.0.result.borrow().as_ref() {
            return Poll::Ready(r.clone());
        }
        self.0.wakers.borrow_mut().push(cx.waker().clone());
        Poll::Pending
    }
}

pub struct Server {
    pub resolver: Rc<Resolver>,
    pub zones: Vec<Zone>,
    pub ens: Option<Rc<Ens>>,
    pub ens_alias: bool,
    pub onion: Option<OnionConfig>,
    pub recursion: bool,
    pub counters: RefCell<Counters>,
    pub started_ms: u64,
    /// The host refuses this guest's UDP sends (the platform's transparent
    /// egress denies every UDP send, replies included): UDP/53 cannot answer.
    pub udp_replies_blocked: Cell<bool>,
    udp_rate: f64,
    udp_burst: f64,
    buckets: RefCell<HashMap<String, (f64, u64)>>,
    slip: Cell<u64>,
    inflight: RefCell<HashMap<(Name, u16, bool, bool), Rc<Slot>>>,
    eth: Name,
    ens_tld: Name,
    onion_tld: Name,
    localhost: Name,
}

pub enum Admit {
    Yes,
    /// Answer with an empty truncated response: a real client retries over
    /// TCP, a spoofed victim gets nothing amplified (RRL "slip").
    Slip,
    Drop,
}

impl Server {
    pub fn new(
        resolver: Rc<Resolver>,
        zones: Vec<Zone>,
        ens: Option<Rc<Ens>>,
        ens_alias: bool,
        onion: Option<OnionConfig>,
        recursion: bool,
        udp_rate: f64,
        udp_burst: f64,
    ) -> Rc<Server> {
        Rc::new(Server {
            resolver,
            zones,
            ens,
            ens_alias,
            onion,
            recursion,
            counters: RefCell::default(),
            started_ms: crate::net::now_ms(),
            udp_replies_blocked: Cell::new(false),
            udp_rate,
            udp_burst,
            buckets: RefCell::default(),
            slip: Cell::new(0),
            inflight: RefCell::default(),
            eth: Name::parse("eth").unwrap(),
            ens_tld: Name::parse("ens").unwrap(),
            onion_tld: Name::parse("onion").unwrap(),
            localhost: Name::parse("localhost").unwrap(),
        })
    }

    /// Per-flow token bucket for UDP. The platform's UDP relay gives every
    /// client address its own flow (seen here as its own loopback port), so
    /// this limits per client, which is what blunts reflection attacks.
    pub fn admit_udp(&self, flow: &str) -> Admit {
        let now = crate::net::now_ms();
        let mut b = self.buckets.borrow_mut();
        if b.len() > 50_000 {
            b.retain(|_, (_, t)| now.saturating_sub(*t) < 10_000);
        }
        let (tokens, last) = b.entry(flow.to_string()).or_insert((self.udp_burst, now));
        *tokens = (*tokens + (now.saturating_sub(*last)) as f64 / 1000.0 * self.udp_rate).min(self.udp_burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            return Admit::Yes;
        }
        self.counters.borrow_mut().rate_limited += 1;
        let s = self.slip.get() + 1;
        self.slip.set(s);
        if s % 2 == 0 {
            Admit::Slip
        } else {
            Admit::Drop
        }
    }

    fn zone_for(&self, name: &Name) -> Option<&Zone> {
        self.zones.iter().filter(|z| name.is_subdomain_of(&z.apex)).max_by_key(|z| z.apex.label_count())
    }

    /// One hop: the answer for exactly (name, qtype) from whichever source owns it.
    async fn source(self: &Rc<Self>, name: &Name, qtype: u16, dnssec_ok: bool, ctx: &Rc<Ctx>) -> Answer {
        if let Some(z) = self.zone_for(name) {
            self.counters.borrow_mut().zone += 1;
            return z.answer(name, qtype, dnssec_ok);
        }
        if let Some(ens) = &self.ens {
            if self.ens_alias && name.is_subdomain_of(&self.ens_tld) && name.label_count() > 1 {
                // `<name>.ens` is a convenience spelling of `<name>.eth`
                let target = name.replace_suffix(&self.ens_tld, &self.eth).unwrap();
                let mut a = Answer::negative(rcode::NOERROR, 300, "ens");
                a.answer.push(Record::new(name.clone(), ty::CNAME, 300, target.wire().to_vec()));
                return a;
            }
            if name.is_subdomain_of(&self.eth) {
                self.counters.borrow_mut().ens += 1;
                return ens.answer(name, qtype).await;
            }
        }
        if name.is_subdomain_of(&self.onion_tld) {
            if let Some(o) = &self.onion {
                self.counters.borrow_mut().onion += 1;
                return onion::answer(o, name, qtype);
            }
        }
        if name.is_subdomain_of(&self.localhost) {
            // RFC 6761
            let mut a = Answer::negative(rcode::NOERROR, 3600, "local");
            a.aa = true;
            match qtype {
                ty::A => a.answer.push(Record::new(name.clone(), ty::A, 3600, vec![127, 0, 0, 1])),
                ty::AAAA => {
                    let mut v = vec![0u8; 16];
                    v[15] = 1;
                    a.answer.push(Record::new(name.clone(), ty::AAAA, 3600, v))
                }
                _ => {}
            }
            return a;
        }
        if !self.recursion {
            let mut a = Answer::negative(rcode::REFUSED, 0, "policy");
            a.ede = Some((ede::NOT_AUTHORITATIVE, "recursion is disabled on this server".into()));
            return a;
        }
        self.resolver.lookup(name, qtype, ctx, 0).await
    }

    /// Resolve (qname, qtype), following CNAMEs across sources; identical
    /// concurrent questions share one resolution.
    pub async fn resolve(self: &Rc<Self>, qname: &Name, qtype: u16, dnssec_ok: bool, cd: bool) -> Resolution {
        let key = (qname.to_lowercase(), qtype, dnssec_ok, cd);
        let existing = self.inflight.borrow().get(&key).cloned();
        if let Some(slot) = existing {
            return WaitSlot(slot).await;
        }
        let slot = Rc::new(Slot { result: RefCell::new(None), wakers: RefCell::new(Vec::new()) });
        self.inflight.borrow_mut().insert(key.clone(), slot.clone());
        let r = self.resolve_chain(qname, qtype, dnssec_ok).await;
        self.inflight.borrow_mut().remove(&key);
        *slot.result.borrow_mut() = Some(r.clone());
        for w in slot.wakers.borrow_mut().drain(..) {
            w.wake();
        }
        r
    }

    async fn resolve_chain(self: &Rc<Self>, qname: &Name, qtype: u16, dnssec_ok: bool) -> Resolution {
        let ctx = Ctx::new(Duration::from_secs(9));
        let mut out = Resolution {
            rcode: rcode::NOERROR,
            answer: Vec::new(),
            authority: Vec::new(),
            additional: Vec::new(),
            sec: Sec::Secure,
            ede: None,
            aa: true,
            sources: Vec::new(),
        };
        let mut name = qname.clone();
        let mut seen: Vec<Name> = vec![name.clone()];
        for _ in 0..12 {
            let a = self.source(&name, qtype, dnssec_ok, &ctx).await;
            if !out.sources.contains(&a.source) {
                out.sources.push(a.source);
            }
            if a.sec > out.sec {
                out.sec = a.sec;
            }
            if a.ede.is_some() && (out.ede.is_none() || a.sec >= Sec::Indeterminate) {
                out.ede = a.ede.clone();
            }
            // AA only for what this server is the authority for
            out.aa &= a.aa && matches!(a.source, "zone" | "onion" | "local");
            for r in &a.answer {
                if !out.answer.contains(r) {
                    out.answer.push(r.clone());
                }
            }
            out.rcode = a.rcode;
            out.authority = a.authority.clone();
            out.additional = a.additional.clone();
            if a.rcode != rcode::NOERROR || qtype == ty::CNAME || !a.rrset(&name, qtype).is_empty() {
                break;
            }
            match a.cname_target(&name) {
                Some(t) if seen.contains(&t) => {
                    out.rcode = rcode::SERVFAIL;
                    out.ede = Some((ede::OTHER, format!("CNAME loop at {t}")));
                    break;
                }
                Some(t) => {
                    seen.push(t.clone());
                    name = t;
                }
                None => break,
            }
        }
        if out.sources.iter().any(|s| *s == "ens" || *s == "onion" || *s == "local" || *s == "policy") && out.sec == Sec::Secure {
            out.sec = Sec::Insecure;
        }
        out
    }

    /// Answer one query. None = send nothing (unparseable, or a response).
    pub async fn handle(self: &Rc<Self>, wire: &[u8], via: Via) -> Option<Vec<u8>> {
        let t0 = crate::net::now_ms();
        {
            let mut c = self.counters.borrow_mut();
            match via {
                Via::Udp => c.udp += 1,
                Via::Tcp => c.tcp += 1,
                Via::Tls => c.tls += 1,
                Via::Https => c.https += 1,
            }
        }
        let q = match Message::parse(wire) {
            Ok(q) => q,
            Err(_) => {
                if wire.len() < 12 || wire[2] & 0x80 != 0 {
                    return None;
                }
                // echo the id with FORMERR
                let mut r = Message { id: u16::from_be_bytes([wire[0], wire[1]]), qr: true, rcode: rcode::FORMERR, ..Default::default() };
                r.rd = wire[2] & 1 != 0;
                return Some(r.to_bytes(512));
            }
        };
        if q.qr {
            return None;
        }
        let mut resp = Message {
            id: q.id,
            qr: true,
            opcode: q.opcode,
            rd: q.rd,
            cd: q.cd,
            ra: self.recursion,
            questions: q.questions.clone(),
            ..Default::default()
        };
        let client_udp = q.edns.as_ref().map(|e| e.udp_size.max(512) as usize).unwrap_or(512);
        let max = match via {
            Via::Udp => client_udp.min(1232),
            _ => 65535,
        };
        if let Some(e) = &q.edns {
            resp.edns = Some(Edns { udp_size: 1232, dnssec_ok: e.dnssec_ok, ..Default::default() });
            if e.version != 0 {
                resp.rcode = rcode::BADVERS;
                return Some(self.finish(resp, &q, via, max, t0));
            }
        }
        if q.opcode != 0 {
            resp.rcode = rcode::NOTIMP;
            return Some(self.finish(resp, &q, via, max, t0));
        }
        let Some(question) = q.questions.first().filter(|_| q.questions.len() == 1) else {
            resp.rcode = rcode::FORMERR;
            return Some(self.finish(resp, &q, via, max, t0));
        };
        let (qname, qtype) = (question.name.clone(), question.qtype);
        if question.qclass == 3 && qtype == ty::TXT {
            // CH TXT version.bind / version.server: what is answering
            let l = qname.lower_string();
            if l == "version.bind." || l == "version.server." {
                resp.aa = true;
                let v = format!("EnclaveDNS {}", env!("CARGO_PKG_VERSION"));
                let mut d = vec![v.len() as u8];
                d.extend_from_slice(v.as_bytes());
                resp.answers.push(Record { name: qname, rtype: ty::TXT, class: 3, ttl: 0, data: d });
                return Some(self.finish(resp, &q, via, max, t0));
            }
        }
        if question.qclass != CLASS_IN {
            resp.rcode = rcode::REFUSED;
            return Some(self.finish(resp, &q, via, max, t0));
        }
        if qtype == ty::AXFR || qtype == ty::IXFR {
            resp.rcode = if via == Via::Udp { rcode::FORMERR } else { rcode::REFUSED };
            resp.add_ede(ede::NOT_SUPPORTED, "zone transfers are not offered");
            return Some(self.finish(resp, &q, via, max, t0));
        }
        let dnssec_ok = q.dnssec_ok();
        if qtype == ty::ANY && self.zone_for(&qname).is_none() {
            // RFC 8482: no ANY expansion through the resolver
            let mut d = vec![7];
            d.extend_from_slice(b"RFC8482");
            d.push(0);
            resp.answers.push(Record::new(qname, ty::HINFO, 3600, d));
            return Some(self.finish(resp, &q, via, max, t0));
        }
        let r = self.resolve(&qname, qtype, dnssec_ok, q.cd).await;
        resp.aa = r.aa;
        let bogus = r.sec == Sec::Bogus;
        if bogus && !q.cd {
            resp.rcode = rcode::SERVFAIL;
        } else {
            resp.rcode = r.rcode;
            let keep = |rec: &Record| dnssec_ok || !matches!(rec.rtype, ty::RRSIG | ty::NSEC | ty::NSEC3) || rec.rtype == qtype;
            resp.answers = r.answer.iter().filter(|x| keep(x)).cloned().collect();
            resp.authority = r.authority.iter().filter(|x| keep(x)).cloned().collect();
            resp.additional = r.additional.clone();
            // RFC 4035/6840: AD only for data this server validated (or signed)
            resp.ad = r.sec == Sec::Secure && (dnssec_ok || q.ad) && resp.rcode != rcode::SERVFAIL;
        }
        if let Some((c, w)) = &r.ede {
            resp.add_ede(*c, w);
        }
        Some(self.finish(resp, &q, via, max, t0))
    }

    fn finish(&self, resp: Message, q: &Message, via: Via, max: usize, t0: u64) -> Vec<u8> {
        {
            let mut c = self.counters.borrow_mut();
            match resp.rcode {
                rcode::NOERROR => c.noerror += 1,
                rcode::NXDOMAIN => c.nxdomain += 1,
                rcode::SERVFAIL => c.servfail += 1,
                _ => c.refused += 1,
            }
            if resp.ad {
                c.ad += 1;
            }
            c.answered += 1;
            c.latency_ms_total += crate::net::now_ms().saturating_sub(t0);
        }
        // RFC 7830/8467: pad encrypted responses when the client padded
        if matches!(via, Via::Tls | Via::Https) && q.edns_option(OPT_PADDING).is_some() {
            return resp.to_bytes_padded(max, 468);
        }
        resp.to_bytes(max)
    }

    /// An empty truncated reply (RRL slip).
    pub fn slip_reply(wire: &[u8]) -> Option<Vec<u8>> {
        let q = Message::parse(wire).ok()?;
        let r = Message { id: q.id, qr: true, tc: true, rd: q.rd, questions: q.questions, ..Default::default() };
        Some(r.to_bytes(512))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slip_is_truncated_echo() {
        let n = Name::parse("www.enclavedns.test").unwrap();
        let q = Message::query(7, &n, ty::A, true, false, false).to_bytes(512);
        let r = Server::slip_reply(&q).expect("a slip reply");
        let m = Message::parse(&r).unwrap();
        assert!(m.tc && m.qr && m.id == 7 && m.answers.is_empty());
    }
}
