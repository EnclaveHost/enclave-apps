//! Hosted zones: authoritative answers from the app config, signed online.
//!
//! One ECDSA P-256 key per zone (a combined signing key, flags 257) derived
//! from the deployment's seed secret, so the DS record published at the
//! parent survives restarts. Every RRset is signed when first asked for and
//! the signature reused for a day. Nonexistence uses Compact Denial of
//! Existence (RFC 9824): one NSEC at the query name whose next name is the
//! immediate successor, so nothing about the rest of the zone is revealed
//! and no zone walking is possible.

use crate::dnssec::{make_bitmap, ZoneSigner};
use crate::name::Name;
use crate::rdata;
use crate::resolver::{Answer, Sec};
use crate::wire::{rcode, ty, Record};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

pub struct ZoneConfig {
    pub name: String,
    pub records: Vec<String>,
    pub ttl: u32,
    pub dnssec: bool,
}

type RRset = (u32, Vec<Vec<u8>>);

pub struct Zone {
    pub apex: Name,
    /// owner -> type -> (ttl, rdatas); canonical order
    nodes: BTreeMap<Name, BTreeMap<u16, RRset>>,
    pub signer: Option<ZoneSigner>,
    pub ephemeral_key: bool,
    neg_ttl: u32,
    /// (owner, type, ttl, rdatas) -> (RRSIG, signed at)
    sigs: RefCell<HashMap<(Name, u16, u32, Vec<Vec<u8>>), (Record, u64)>>,
}

const SIG_REUSE_SECS: u64 = 86400;
const SIG_VALIDITY: u32 = 7 * 86400;

impl Zone {
    pub fn build(cfg: &ZoneConfig, seed: Option<&[u8]>) -> Result<Zone, String> {
        let apex = Name::parse(&cfg.name)?;
        let mut nodes: BTreeMap<Name, BTreeMap<u16, RRset>> = BTreeMap::new();
        for (i, line) in cfg.records.iter().enumerate() {
            let r = rdata::parse_line(line, &apex, cfg.ttl).map_err(|e| format!("{} record {}: {e}", apex, i + 1))?;
            let Some(r) = r else { continue };
            if r.rtype == ty::SOA && r.name != apex {
                return Err(format!("{}: SOA only at the apex", apex));
            }
            let set = nodes.entry(r.name.to_lowercase()).or_default().entry(r.rtype).or_insert((r.ttl, Vec::new()));
            set.0 = set.0.min(r.ttl);
            if !set.1.contains(&r.data) {
                set.1.push(r.data);
            }
        }
        // CNAME stands alone (RFC 1034 §3.6.2; RFC 4035 allows RRSIG/NSEC next to it)
        for (owner, types) in &nodes {
            if types.contains_key(&ty::CNAME) && types.len() > 1 {
                return Err(format!("{owner} has a CNAME and other data"));
            }
            if types.get(&ty::CNAME).is_some_and(|s| s.1.len() > 1) {
                return Err(format!("{owner} has more than one CNAME"));
            }
        }
        if nodes.get(&apex.to_lowercase()).is_some_and(|t| t.contains_key(&ty::CNAME)) {
            return Err(format!("{apex}: no CNAME at a zone apex"));
        }
        let apex_lc = apex.to_lowercase();
        if !nodes.get(&apex_lc).is_some_and(|t| t.contains_key(&ty::SOA)) {
            let mname = apex.prepend(b"ns1").map_err(|e| e.to_string())?;
            let rname = apex.prepend(b"hostmaster").map_err(|e| e.to_string())?;
            let mut d = mname.wire().to_vec();
            d.extend_from_slice(rname.wire());
            for v in [1u32, 7200, 3600, 1209600, 300] {
                d.extend_from_slice(&v.to_be_bytes());
            }
            nodes.entry(apex_lc.clone()).or_default().insert(ty::SOA, (cfg.ttl, vec![d]));
        }
        let soa = &nodes[&apex_lc][&ty::SOA];
        let min = u32::from_be_bytes(soa.1[0][soa.1[0].len() - 4..].try_into().unwrap());
        let neg_ttl = soa.0.min(min);
        let (signer, ephemeral_key) = if cfg.dnssec {
            match seed {
                Some(s) => (Some(ZoneSigner::from_seed(&apex, s)), false),
                None => {
                    let mut s = [0u8; 32];
                    getrandom::getrandom(&mut s).map_err(|e| e.to_string())?;
                    (Some(ZoneSigner::from_seed(&apex, &s)), true)
                }
            }
        } else {
            (None, false)
        };
        if let Some(sg) = &signer {
            let apex_node = nodes.entry(apex_lc.clone()).or_default();
            apex_node.insert(ty::DNSKEY, (3600, vec![sg.dnskey_rdata.clone()]));
            // RFC 7344/8078: lets a registry that scans for them install the DS itself
            apex_node.insert(ty::CDNSKEY, (3600, vec![sg.dnskey_rdata.clone()]));
            apex_node.insert(ty::CDS, (3600, vec![sg.ds_rdata()]));
        }
        Ok(Zone { apex, nodes, signer, ephemeral_key, neg_ttl, sigs: RefCell::default() })
    }

    pub fn record_count(&self) -> usize {
        self.nodes.values().flat_map(|t| t.values()).map(|s| s.1.len()).sum()
    }

    /// Every record, presentation form (for the status page).
    pub fn listing(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (owner, types) in &self.nodes {
            for (t, (ttl, rds)) in types {
                for d in rds {
                    out.push(format!("{owner} {ttl} IN {} {}", ty::name(*t), rdata::to_text(*t, d)));
                }
            }
        }
        out
    }

    pub fn ds_text(&self) -> Option<String> {
        self.signer.as_ref().map(|s| format!("{} IN DS {}", self.apex, rdata::to_text(ty::DS, &s.ds_rdata())))
    }

    pub fn dnskey_text(&self) -> Option<String> {
        self.signer.as_ref().map(|s| format!("{} IN DNSKEY {}", self.apex, rdata::to_text(ty::DNSKEY, &s.dnskey_rdata)))
    }

    fn rrsig(&self, owner: &Name, rtype: u16, ttl: u32, rdatas: &[Vec<u8>]) -> Option<Record> {
        let s = self.signer.as_ref()?;
        let now = crate::net::now_unix();
        let key = (owner.to_lowercase(), rtype, ttl, rdatas.to_vec());
        if let Some((r, made)) = self.sigs.borrow().get(&key) {
            if now.saturating_sub(*made) < SIG_REUSE_SECS {
                return Some(r.clone());
            }
        }
        let inception = (now as u32).wrapping_sub(3600);
        let r = s.sign(owner, rtype, ttl, rdatas, inception, inception.wrapping_add(SIG_VALIDITY));
        let mut sigs = self.sigs.borrow_mut();
        if sigs.len() > 20_000 {
            sigs.clear();
        }
        sigs.insert(key, (r.clone(), now));
        Some(r)
    }

    fn push_set(&self, out: &mut Vec<Record>, owner: &Name, rtype: u16, set: &RRset, dnssec_ok: bool) {
        for d in &set.1 {
            out.push(Record::new(owner.clone(), rtype, set.0, d.clone()));
        }
        if dnssec_ok {
            if let Some(sig) = self.rrsig(owner, rtype, set.0, &set.1) {
                out.push(sig);
            }
        }
    }

    fn soa(&self, out: &mut Vec<Record>, dnssec_ok: bool) {
        let set = &self.nodes[&self.apex.to_lowercase()][&ty::SOA];
        // RFC 2308: the SOA in a negative answer carries the negative TTL
        let s = (self.neg_ttl, set.1.clone());
        self.push_set(out, &self.apex, ty::SOA, &s, dnssec_ok);
    }

    /// RFC 9824 compact denial: NSEC at `owner` -> its immediate successor.
    fn compact_nsec(&self, out: &mut Vec<Record>, owner: &Name, types: &[u16]) {
        let next = owner.prepend(&[0]).unwrap_or_else(|_| owner.clone());
        let mut ts = types.to_vec();
        ts.extend_from_slice(&[ty::RRSIG, ty::NSEC]);
        let mut d = next.wire().to_vec();
        d.extend_from_slice(&make_bitmap(&ts));
        let set = (self.neg_ttl, vec![d]);
        self.push_set(out, owner, ty::NSEC, &set, true);
    }

    fn exists(&self, n: &Name) -> bool {
        self.nodes.contains_key(&n.to_lowercase())
    }

    /// An empty non-terminal: no records of its own, records beneath. In
    /// canonical order a name's descendants come right after it.
    fn is_ent(&self, n: &Name) -> bool {
        use std::ops::Bound::{Excluded, Unbounded};
        let lc = n.to_lowercase();
        self.nodes.range((Excluded(lc.clone()), Unbounded)).next().is_some_and(|(k, _)| k.is_subdomain_of(&lc))
    }

    pub fn answer(&self, qname: &Name, qtype: u16, dnssec_ok: bool) -> Answer {
        let signed = dnssec_ok && self.signer.is_some();
        let mut a = Answer {
            rcode: rcode::NOERROR,
            answer: Vec::new(),
            authority: Vec::new(),
            additional: Vec::new(),
            sec: Sec::Insecure,
            ede: None,
            ttl: 0,
            aa: true,
            source: "zone",
        };
        // a delegation on the way down: refer (DS itself is answered here, the parent side)
        let mut cut = qname.clone();
        while cut.label_count() > self.apex.label_count() {
            if let Some(ns) = self.nodes.get(&cut.to_lowercase()).and_then(|t| t.get(&ty::NS)) {
                if !(qtype == ty::DS && cut == *qname) {
                    a.aa = false;
                    self.push_set(&mut a.authority, &cut, ty::NS, ns, false);
                    if let Some(ds) = self.nodes[&cut.to_lowercase()].get(&ty::DS) {
                        self.push_set(&mut a.authority, &cut, ty::DS, ds, signed);
                    } else if signed {
                        // no DS: an NSEC at the cut that shows NS and no DS (insecure delegation)
                        self.compact_nsec(&mut a.authority, &cut, &[ty::NS]);
                    }
                    for n in ns.1.iter().filter_map(|d| Name::read(d, &mut 0).ok()) {
                        if n.is_subdomain_of(&cut) {
                            for t in [ty::A, ty::AAAA] {
                                if let Some(g) = self.nodes.get(&n.to_lowercase()).and_then(|x| x.get(&t)) {
                                    self.push_set(&mut a.additional, &n, t, g, false);
                                }
                            }
                        }
                    }
                    a.ttl = ns.0;
                    return a;
                }
            }
            cut = cut.parent().unwrap();
        }
        if qtype == ty::ANY {
            // RFC 8482: a minimal answer instead of everything
            let mut d = vec![7];
            d.extend_from_slice(b"RFC8482");
            d.push(0);
            self.push_set(&mut a.answer, qname, ty::HINFO, &(3600, vec![d]), signed);
            a.ttl = 3600;
            return a;
        }
        let node = self.nodes.get(&qname.to_lowercase()).cloned().or_else(|| self.wildcard(qname));
        match node {
            Some(types) => {
                if let Some(set) = types.get(&qtype) {
                    self.push_set(&mut a.answer, qname, qtype, set, signed);
                    a.ttl = set.0;
                } else if let Some(set) = types.get(&ty::CNAME) {
                    self.push_set(&mut a.answer, qname, ty::CNAME, set, signed);
                    a.ttl = set.0;
                } else {
                    self.soa(&mut a.authority, signed);
                    if signed {
                        let present: Vec<u16> = types.keys().copied().collect();
                        self.compact_nsec(&mut a.authority, qname, &present);
                    }
                    a.ttl = self.neg_ttl;
                }
            }
            None if self.is_ent(qname) => {
                self.soa(&mut a.authority, signed);
                if signed {
                    self.compact_nsec(&mut a.authority, qname, &[]);
                }
                a.ttl = self.neg_ttl;
            }
            None => {
                self.soa(&mut a.authority, signed);
                if signed {
                    // compact denial: NOERROR + NSEC with NXNAME (RFC 9824)
                    self.compact_nsec(&mut a.authority, qname, &[ty::NXNAME]);
                } else {
                    a.rcode = rcode::NXDOMAIN;
                }
                a.ttl = self.neg_ttl;
            }
        }
        if self.signer.is_some() {
            a.sec = Sec::Secure;
        }
        a
    }

    /// RFC 4592 wildcard: the closest encloser's `*` child, if it has data.
    fn wildcard(&self, qname: &Name) -> Option<BTreeMap<u16, RRset>> {
        let mut ce = qname.parent()?;
        loop {
            if !ce.is_subdomain_of(&self.apex) {
                return None;
            }
            if self.exists(&ce) || self.is_ent(&ce) || ce == self.apex {
                let wc = ce.prepend(b"*").ok()?;
                return self.nodes.get(&wc.to_lowercase()).cloned();
            }
            ce = ce.parent()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dnssec::{self, Claim, Denial, Dnskey, Nsec, Rrsig};

    fn zone() -> Zone {
        Zone::build(
            &ZoneConfig {
                name: "example.test".into(),
                records: vec![
                    "@ NS ns1".into(),
                    "ns1 A 192.0.2.53".into(),
                    "www A 192.0.2.1".into(),
                    "www AAAA 2001:db8::1".into(),
                    "alias CNAME www".into(),
                    "a.b.c TXT deep".into(),
                    "*.wild TXT \"wildcard\"".into(),
                    "sub NS ns.sub".into(),
                    "ns.sub A 192.0.2.99".into(),
                ],
                ttl: 300,
                dnssec: true,
            },
            Some(b"0123456789abcdef0123456789abcdef"),
        )
        .unwrap()
    }

    fn n(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    fn check_sigs(z: &Zone, recs: &[Record]) {
        let key = Dnskey::parse(&z.signer.as_ref().unwrap().dnskey_rdata).unwrap();
        let now = crate::net::now_unix() as u32;
        for set in crate::wire::rrsets(recs).into_iter().filter(|s| s[0].rtype != ty::RRSIG) {
            let sigs: Vec<Rrsig> = recs
                .iter()
                .filter(|r| r.rtype == ty::RRSIG && r.name == set[0].name && r.covered() == set[0].rtype)
                .filter_map(|r| Rrsig::parse(&r.data))
                .collect();
            let rd: Vec<Vec<u8>> = set.iter().map(|r| r.data.clone()).collect();
            dnssec::verify_rrset(&set[0].name, set[0].rtype, 1, &rd, &sigs, &[key.clone()], &z.apex, now).unwrap();
        }
    }

    #[test]
    fn positive_and_signed() {
        let z = zone();
        let a = z.answer(&n("WWW.example.test"), ty::A, true);
        assert_eq!(a.rcode, rcode::NOERROR);
        assert_eq!(a.answer.iter().filter(|r| r.rtype == ty::A).count(), 1);
        check_sigs(&z, &a.answer);
        let k = z.answer(&n("example.test"), ty::DNSKEY, true);
        check_sigs(&z, &k.answer);
        assert!(z.ds_text().unwrap().contains(" 13 2 "));
    }

    #[test]
    fn denial_is_compact_and_provable() {
        let z = zone();
        let a = z.answer(&n("nope.example.test"), ty::A, true);
        assert_eq!(a.rcode, rcode::NOERROR, "compact denial answers NOERROR");
        check_sigs(&z, &a.authority);
        let nsecs: Vec<Nsec> = a.authority.iter().filter(|r| r.rtype == ty::NSEC).filter_map(|r| Nsec::parse(&r.name, &r.data)).collect();
        assert_eq!(dnssec::prove(&n("nope.example.test"), ty::A, Claim::NoData, &nsecs, &[]), Denial::Secure);
        assert!(dnssec::bitmap_has(&nsecs[0].bitmap, ty::NXNAME));
        // unsigned query: a plain NXDOMAIN
        assert_eq!(z.answer(&n("nope.example.test"), ty::A, false).rcode, rcode::NXDOMAIN);
        // NODATA at an existing name lists what is there
        let a = z.answer(&n("www.example.test"), ty::MX, true);
        let nsecs: Vec<Nsec> = a.authority.iter().filter(|r| r.rtype == ty::NSEC).filter_map(|r| Nsec::parse(&r.name, &r.data)).collect();
        assert_eq!(dnssec::prove(&n("www.example.test"), ty::MX, Claim::NoData, &nsecs, &[]), Denial::Secure);
        assert!(dnssec::bitmap_has(&nsecs[0].bitmap, ty::AAAA));
    }

    #[test]
    fn ent_wildcard_cname_delegation() {
        let z = zone();
        let ent = z.answer(&n("b.c.example.test"), ty::A, false);
        assert_eq!(ent.rcode, rcode::NOERROR);
        assert!(ent.answer.is_empty());
        let w = z.answer(&n("anything.wild.example.test"), ty::TXT, true);
        assert_eq!(w.answer[0].name, n("anything.wild.example.test"));
        check_sigs(&z, &w.answer);
        let c = z.answer(&n("alias.example.test"), ty::A, false);
        assert_eq!(c.answer[0].rtype, ty::CNAME);
        let d = z.answer(&n("host.sub.example.test"), ty::A, true);
        assert!(!d.aa && d.answer.is_empty());
        assert!(d.authority.iter().any(|r| r.rtype == ty::NS));
        assert!(d.additional.iter().any(|r| r.rtype == ty::A), "glue");
        // the NSEC at an unsigned delegation shows NS and no DS
        let nsec = d.authority.iter().find(|r| r.rtype == ty::NSEC).unwrap();
        let ns = Nsec::parse(&nsec.name, &nsec.data).unwrap();
        assert_eq!(dnssec::prove(&n("sub.example.test"), ty::DS, Claim::NoData, &[ns], &[]), Denial::Insecure(0, "sub.example.test. is an unsigned delegation".into()));
    }

    #[test]
    fn bad_zones_refused() {
        let bad = |recs: &[&str]| {
            Zone::build(&ZoneConfig { name: "x.test".into(), records: recs.iter().map(|s| s.to_string()).collect(), ttl: 60, dnssec: false }, None)
                .is_err()
        };
        assert!(bad(&["a CNAME b", "a A 192.0.2.1"]));
        assert!(bad(&["@ CNAME elsewhere.org."]));
        assert!(bad(&["www.other.org. A 192.0.2.1"]));
        assert!(!bad(&["a CNAME b"]));
    }
}
