//! The DNS message format (RFC 1035), EDNS(0) (RFC 6891), Extended DNS
//! Errors (RFC 8914). Record data is kept uncompressed with its original
//! case, which is what DNSSEC verification and re-serialization both need.

use crate::name::Name;
use std::collections::HashMap;

pub mod ty {
    pub const A: u16 = 1;
    pub const NS: u16 = 2;
    pub const CNAME: u16 = 5;
    pub const SOA: u16 = 6;
    pub const PTR: u16 = 12;
    pub const HINFO: u16 = 13;
    pub const MX: u16 = 15;
    pub const TXT: u16 = 16;
    pub const RP: u16 = 17;
    pub const AFSDB: u16 = 18;
    pub const SIG: u16 = 24;
    pub const AAAA: u16 = 28;
    pub const LOC: u16 = 29;
    pub const SRV: u16 = 33;
    pub const NAPTR: u16 = 35;
    pub const KX: u16 = 36;
    pub const DNAME: u16 = 39;
    pub const OPT: u16 = 41;
    pub const DS: u16 = 43;
    pub const SSHFP: u16 = 44;
    pub const RRSIG: u16 = 46;
    pub const NSEC: u16 = 47;
    pub const DNSKEY: u16 = 48;
    pub const NSEC3: u16 = 50;
    pub const NSEC3PARAM: u16 = 51;
    pub const TLSA: u16 = 52;
    pub const CDS: u16 = 59;
    pub const CDNSKEY: u16 = 60;
    pub const OPENPGPKEY: u16 = 61;
    pub const SVCB: u16 = 64;
    pub const HTTPS: u16 = 65;
    /// RFC 9824 (Compact Denial of Existence): signals a nonexistent name.
    pub const NXNAME: u16 = 128;
    pub const IXFR: u16 = 251;
    pub const AXFR: u16 = 252;
    pub const ANY: u16 = 255;
    pub const URI: u16 = 256;
    pub const CAA: u16 = 257;

    const NAMES: &[(u16, &str)] = &[
        (A, "A"), (NS, "NS"), (CNAME, "CNAME"), (SOA, "SOA"), (PTR, "PTR"), (HINFO, "HINFO"),
        (MX, "MX"), (TXT, "TXT"), (RP, "RP"), (AFSDB, "AFSDB"), (SIG, "SIG"), (AAAA, "AAAA"),
        (LOC, "LOC"), (SRV, "SRV"), (NAPTR, "NAPTR"), (KX, "KX"), (DNAME, "DNAME"), (OPT, "OPT"),
        (DS, "DS"), (SSHFP, "SSHFP"), (RRSIG, "RRSIG"), (NSEC, "NSEC"), (DNSKEY, "DNSKEY"),
        (NSEC3, "NSEC3"), (NSEC3PARAM, "NSEC3PARAM"), (TLSA, "TLSA"), (CDS, "CDS"),
        (CDNSKEY, "CDNSKEY"), (OPENPGPKEY, "OPENPGPKEY"), (SVCB, "SVCB"), (HTTPS, "HTTPS"),
        (NXNAME, "NXNAME"), (IXFR, "IXFR"), (AXFR, "AXFR"), (ANY, "ANY"), (URI, "URI"), (CAA, "CAA"),
    ];

    pub fn name(t: u16) -> String {
        NAMES.iter().find(|(v, _)| *v == t).map(|(_, n)| n.to_string()).unwrap_or_else(|| format!("TYPE{t}"))
    }

    pub fn parse(s: &str) -> Option<u16> {
        let u = s.trim().to_ascii_uppercase();
        if let Some(n) = u.strip_prefix("TYPE") {
            return n.parse().ok();
        }
        if let Ok(n) = u.parse::<u16>() {
            return Some(n);
        }
        NAMES.iter().find(|(_, n)| *n == u).map(|(v, _)| *v)
    }

    /// Meta-types and query-only types a zone can never hold.
    pub fn is_meta(t: u16) -> bool {
        t == OPT || (128..=255).contains(&t)
    }
}

pub mod rcode {
    pub const NOERROR: u16 = 0;
    pub const FORMERR: u16 = 1;
    pub const SERVFAIL: u16 = 2;
    pub const NXDOMAIN: u16 = 3;
    pub const NOTIMP: u16 = 4;
    pub const REFUSED: u16 = 5;
    pub const BADVERS: u16 = 16;

    pub fn name(r: u16) -> &'static str {
        match r {
            0 => "NOERROR",
            1 => "FORMERR",
            2 => "SERVFAIL",
            3 => "NXDOMAIN",
            4 => "NOTIMP",
            5 => "REFUSED",
            16 => "BADVERS",
            _ => "RCODE?",
        }
    }
}

/// RFC 8914 info codes this server emits.
pub mod ede {
    pub const OTHER: u16 = 0;
    pub const UNSUPPORTED_DNSKEY_ALG: u16 = 1;
    pub const DNSSEC_INDETERMINATE: u16 = 5;
    pub const DNSSEC_BOGUS: u16 = 6;
    pub const SIGNATURE_EXPIRED: u16 = 7;
    pub const SIGNATURE_NOT_YET_VALID: u16 = 8;
    pub const DNSKEY_MISSING: u16 = 9;
    pub const RRSIGS_MISSING: u16 = 10;
    pub const NSEC_MISSING: u16 = 12;
    pub const BLOCKED: u16 = 15;
    pub const NOT_AUTHORITATIVE: u16 = 20;
    pub const NOT_SUPPORTED: u16 = 21;
    pub const NO_REACHABLE_AUTHORITY: u16 = 22;
    pub const NETWORK_ERROR: u16 = 23;
    pub const INVALID_DATA: u16 = 24;
    pub const UNSUPPORTED_NSEC3_ITERATIONS: u16 = 27;
}

pub const CLASS_IN: u16 = 1;
pub const OPT_EDE: u16 = 15;
pub const OPT_PADDING: u16 = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: Name,
    pub rtype: u16,
    pub class: u16,
    pub ttl: u32,
    /// Uncompressed, original case.
    pub data: Vec<u8>,
}

impl Record {
    pub fn new(name: Name, rtype: u16, ttl: u32, data: Vec<u8>) -> Record {
        Record { name, rtype, class: CLASS_IN, ttl, data }
    }

    /// The type an RRSIG covers (0 for any other record).
    pub fn covered(&self) -> u16 {
        if self.rtype == ty::RRSIG && self.data.len() >= 2 {
            u16::from_be_bytes([self.data[0], self.data[1]])
        } else {
            0
        }
    }

    /// The single name in NS/CNAME/DNAME/PTR rdata.
    pub fn target(&self) -> Option<Name> {
        let mut p = 0;
        match self.rtype {
            ty::NS | ty::CNAME | ty::DNAME | ty::PTR => Name::read(&self.data, &mut p).ok(),
            ty::MX => {
                p = 2;
                Name::read(&self.data, &mut p).ok()
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Edns {
    pub udp_size: u16,
    pub ext_rcode: u8,
    pub version: u8,
    pub dnssec_ok: bool,
    pub options: Vec<(u16, Vec<u8>)>,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub name: Name,
    pub qtype: u16,
    pub qclass: u16,
}

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub id: u16,
    pub qr: bool,
    pub opcode: u8,
    pub aa: bool,
    pub tc: bool,
    pub rd: bool,
    pub ra: bool,
    pub ad: bool,
    pub cd: bool,
    /// Full 12-bit rcode (low 4 bits in the header, high 8 in OPT).
    pub rcode: u16,
    pub questions: Vec<Question>,
    pub answers: Vec<Record>,
    pub authority: Vec<Record>,
    pub additional: Vec<Record>,
    pub edns: Option<Edns>,
}

fn rd16(b: &[u8], p: &mut usize) -> Result<u16, String> {
    let v = b.get(*p..*p + 2).ok_or("message truncated")?;
    *p += 2;
    Ok(u16::from_be_bytes([v[0], v[1]]))
}

fn rd32(b: &[u8], p: &mut usize) -> Result<u32, String> {
    let v = b.get(*p..*p + 4).ok_or("message truncated")?;
    *p += 4;
    Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}

/// Re-encode `rdata` (starting at `start` in `msg`, `len` long) with any
/// embedded compressed names expanded. Types per RFC 3597 §4: the RFC 1035
/// well-known types, plus those receivers SHOULD decompress.
fn decompress_rdata(msg: &[u8], rtype: u16, start: usize, len: usize) -> Result<Vec<u8>, String> {
    let end = start + len;
    let raw = msg.get(start..end).ok_or("rdata runs past the message")?;
    // (fixed prefix bytes, names) per type; names follow the prefix in order
    let (prefix, names, char_strings): (usize, usize, usize) = match rtype {
        ty::NS | ty::CNAME | ty::PTR | ty::DNAME | 3 | 4 | 7 | 8 | 9 => (0, 1, 0),
        ty::SOA => (0, 2, 0),
        ty::MX | ty::AFSDB | 21 | ty::KX => (2, 1, 0),
        14 | ty::RP => (0, 2, 0),
        26 => (2, 2, 0),
        ty::SRV => (6, 1, 0),
        ty::NAPTR => (4, 1, 3),
        ty::RRSIG | ty::SIG => (18, 1, 0),
        ty::NSEC => (0, 1, 0),
        _ => return Ok(raw.to_vec()),
    };
    let mut out = Vec::with_capacity(len + 16);
    let mut p = start;
    out.extend_from_slice(msg.get(p..p + prefix).ok_or("rdata too short")?);
    p += prefix;
    for _ in 0..char_strings {
        let l = *msg.get(p).ok_or("rdata too short")? as usize;
        out.extend_from_slice(msg.get(p..p + 1 + l).ok_or("rdata too short")?);
        p += 1 + l;
    }
    for _ in 0..names {
        let n = Name::read(msg, &mut p)?;
        out.extend_from_slice(n.wire());
    }
    if p > end {
        return Err("rdata names overrun rdlength".into());
    }
    out.extend_from_slice(&msg[p..end]);
    Ok(out)
}

impl Message {
    pub fn parse(b: &[u8]) -> Result<Message, String> {
        if b.len() < 12 {
            return Err("message shorter than a header".into());
        }
        let mut p = 0;
        let id = rd16(b, &mut p)?;
        let flags = rd16(b, &mut p)?;
        let qd = rd16(b, &mut p)? as usize;
        let an = rd16(b, &mut p)? as usize;
        let ns = rd16(b, &mut p)? as usize;
        let ar = rd16(b, &mut p)? as usize;
        let mut m = Message {
            id,
            qr: flags & 0x8000 != 0,
            opcode: ((flags >> 11) & 0xF) as u8,
            aa: flags & 0x0400 != 0,
            tc: flags & 0x0200 != 0,
            rd: flags & 0x0100 != 0,
            ra: flags & 0x0080 != 0,
            ad: flags & 0x0020 != 0,
            cd: flags & 0x0010 != 0,
            rcode: flags & 0xF,
            ..Default::default()
        };
        if qd > 4 {
            return Err("too many questions".into());
        }
        for _ in 0..qd {
            let name = Name::read(b, &mut p)?;
            let qtype = rd16(b, &mut p)?;
            let qclass = rd16(b, &mut p)?;
            m.questions.push(Question { name, qtype, qclass });
        }
        for (section, count) in [(0, an), (1, ns), (2, ar)] {
            for _ in 0..count {
                let name = Name::read(b, &mut p)?;
                let rtype = rd16(b, &mut p)?;
                let class = rd16(b, &mut p)?;
                let ttl = rd32(b, &mut p)?;
                let rdlen = rd16(b, &mut p)? as usize;
                if p + rdlen > b.len() {
                    return Err("rdata runs past the message".into());
                }
                if rtype == ty::OPT {
                    if section != 2 || m.edns.is_some() || !name.is_root() {
                        return Err("misplaced or duplicate OPT".into());
                    }
                    let mut e = Edns {
                        udp_size: class,
                        ext_rcode: (ttl >> 24) as u8,
                        version: (ttl >> 16) as u8,
                        dnssec_ok: ttl & 0x8000 != 0,
                        options: Vec::new(),
                    };
                    let mut q = p;
                    while q + 4 <= p + rdlen {
                        let code = u16::from_be_bytes([b[q], b[q + 1]]);
                        let l = u16::from_be_bytes([b[q + 2], b[q + 3]]) as usize;
                        let v = b.get(q + 4..q + 4 + l).ok_or("EDNS option truncated")?;
                        if q + 4 + l > p + rdlen {
                            return Err("EDNS option overruns OPT".into());
                        }
                        e.options.push((code, v.to_vec()));
                        q += 4 + l;
                    }
                    m.rcode |= (e.ext_rcode as u16) << 4;
                    m.edns = Some(e);
                    p += rdlen;
                    continue;
                }
                let data = decompress_rdata(b, rtype, p, rdlen)?;
                p += rdlen;
                let r = Record { name, rtype, class, ttl, data };
                match section {
                    0 => m.answers.push(r),
                    1 => m.authority.push(r),
                    _ => m.additional.push(r),
                }
            }
        }
        Ok(m)
    }

    pub fn dnssec_ok(&self) -> bool {
        self.edns.as_ref().is_some_and(|e| e.dnssec_ok)
    }

    pub fn edns_option(&self, code: u16) -> Option<&[u8]> {
        self.edns.as_ref()?.options.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_slice())
    }

    /// A query for (name, type) with EDNS, DO and the given flags.
    pub fn query(id: u16, name: &Name, qtype: u16, rd: bool, cd: bool, dnssec_ok: bool) -> Message {
        Message {
            id,
            rd,
            cd,
            questions: vec![Question { name: name.clone(), qtype, qclass: CLASS_IN }],
            edns: Some(Edns { udp_size: 1232, dnssec_ok, ..Default::default() }),
            ..Default::default()
        }
    }

    pub fn add_ede(&mut self, code: u16, text: &str) {
        if let Some(e) = self.edns.as_mut() {
            if e.options.iter().any(|(c, _)| *c == OPT_EDE) {
                return;
            }
            let mut v = code.to_be_bytes().to_vec();
            v.extend_from_slice(text.as_bytes().get(..text.len().min(200)).unwrap_or(&[]));
            e.options.push((OPT_EDE, v));
        }
    }

    /// Serialize; if it does not fit `max`, drop the additional section,
    /// then send a truncated (TC) header + question + OPT.
    pub fn to_bytes(&self, max: usize) -> Vec<u8> {
        let full = self.encode(true, true, true);
        if full.len() <= max {
            return full;
        }
        let no_add = self.encode(true, true, false);
        if no_add.len() <= max {
            return no_add;
        }
        let mut t = self.clone();
        t.tc = true;
        t.encode(false, false, false)
    }

    /// RFC 7830/8467 block padding for encrypted transports: pad the whole
    /// message to a multiple of `block`.
    pub fn to_bytes_padded(&self, max: usize, block: usize) -> Vec<u8> {
        let base = self.to_bytes(max);
        if self.edns.is_none() || block == 0 {
            return base;
        }
        let mut m = if base.len() < self.encode(true, true, true).len() {
            // truncated: pad what was actually sent
            Message::parse(&base).unwrap_or_else(|_| self.clone())
        } else {
            self.clone()
        };
        let e = m.edns.as_mut().unwrap();
        e.options.retain(|(c, _)| *c != OPT_PADDING);
        let cur = base.len() + 4;
        let pad = (block - cur % block) % block;
        if cur + pad > max {
            return base;
        }
        e.options.push((OPT_PADDING, vec![0; pad]));
        m.encode(true, true, true)
    }

    fn encode(&self, with_an_ns: bool, with_add: bool, with_add_records: bool) -> Vec<u8> {
        let mut w = Writer { buf: Vec::with_capacity(512), names: HashMap::new() };
        let mut flags: u16 = 0;
        if self.qr {
            flags |= 0x8000;
        }
        flags |= ((self.opcode & 0xF) as u16) << 11;
        if self.aa {
            flags |= 0x0400;
        }
        if self.tc {
            flags |= 0x0200;
        }
        if self.rd {
            flags |= 0x0100;
        }
        if self.ra {
            flags |= 0x0080;
        }
        if self.ad {
            flags |= 0x0020;
        }
        if self.cd {
            flags |= 0x0010;
        }
        flags |= self.rcode & 0xF;
        let (an, ns) = if with_an_ns { (self.answers.len(), self.authority.len()) } else { (0, 0) };
        let add_n = if with_add && with_add_records { self.additional.len() } else { 0 };
        let ar = add_n + self.edns.is_some() as usize;
        for v in [self.id, flags, self.questions.len() as u16, an as u16, ns as u16, ar as u16] {
            w.buf.extend_from_slice(&v.to_be_bytes());
        }
        for q in &self.questions {
            w.name(&q.name, true);
            w.buf.extend_from_slice(&q.qtype.to_be_bytes());
            w.buf.extend_from_slice(&q.qclass.to_be_bytes());
        }
        if with_an_ns {
            for r in self.answers.iter().chain(self.authority.iter()) {
                w.record(r);
            }
        }
        if add_n > 0 {
            for r in &self.additional {
                w.record(r);
            }
        }
        if let Some(e) = &self.edns {
            w.buf.push(0);
            w.buf.extend_from_slice(&ty::OPT.to_be_bytes());
            w.buf.extend_from_slice(&e.udp_size.to_be_bytes());
            let ttl: u32 = (((self.rcode >> 4) as u32 & 0xFF) << 24)
                | ((e.version as u32) << 16)
                | if e.dnssec_ok { 0x8000 } else { 0 };
            w.buf.extend_from_slice(&ttl.to_be_bytes());
            let olen: usize = e.options.iter().map(|(_, v)| 4 + v.len()).sum();
            w.buf.extend_from_slice(&(olen as u16).to_be_bytes());
            for (c, v) in &e.options {
                w.buf.extend_from_slice(&c.to_be_bytes());
                w.buf.extend_from_slice(&(v.len() as u16).to_be_bytes());
                w.buf.extend_from_slice(v);
            }
        }
        w.buf
    }
}

struct Writer {
    buf: Vec<u8>,
    /// lowercase suffix wire -> offset
    names: HashMap<Vec<u8>, u16>,
}

impl Writer {
    fn name(&mut self, n: &Name, compress: bool) {
        let wire = n.wire();
        let mut i = 0;
        while wire[i] != 0 {
            let suffix = wire[i..].to_ascii_lowercase();
            if compress {
                if let Some(&off) = self.names.get(&suffix) {
                    self.buf.extend_from_slice(&(0xC000 | off).to_be_bytes());
                    return;
                }
            }
            let here = self.buf.len();
            if here < 0x3FFF {
                self.names.entry(suffix).or_insert(here as u16);
            }
            let l = wire[i] as usize;
            self.buf.extend_from_slice(&wire[i..i + 1 + l]);
            i += 1 + l;
        }
        self.buf.push(0);
    }

    fn record(&mut self, r: &Record) {
        self.name(&r.name, true);
        self.buf.extend_from_slice(&r.rtype.to_be_bytes());
        self.buf.extend_from_slice(&r.class.to_be_bytes());
        self.buf.extend_from_slice(&r.ttl.to_be_bytes());
        let len_at = self.buf.len();
        self.buf.extend_from_slice(&[0, 0]);
        let start = self.buf.len();
        // RFC 1035 types may carry compressed names; others go verbatim
        let mut p = 0;
        let d = &r.data;
        let ok = (|| -> Result<(), String> {
            match r.rtype {
                ty::NS | ty::CNAME | ty::PTR => {
                    let n = Name::read(d, &mut p)?;
                    self.name(&n, true);
                }
                ty::MX => {
                    self.buf.extend_from_slice(d.get(..2).ok_or("short")?);
                    p = 2;
                    let n = Name::read(d, &mut p)?;
                    self.name(&n, true);
                }
                ty::SOA => {
                    let a = Name::read(d, &mut p)?;
                    let b = Name::read(d, &mut p)?;
                    self.name(&a, true);
                    self.name(&b, true);
                    self.buf.extend_from_slice(d.get(p..).ok_or("short")?);
                    p = d.len();
                }
                _ => {
                    self.buf.extend_from_slice(d);
                    p = d.len();
                }
            }
            if p != d.len() {
                return Err("trailing rdata".into());
            }
            Ok(())
        })();
        if ok.is_err() {
            self.buf.truncate(start);
            self.buf.extend_from_slice(d);
        }
        let len = (self.buf.len() - start) as u16;
        self.buf[len_at..len_at + 2].copy_from_slice(&len.to_be_bytes());
    }
}

/// Group records into RRsets (same owner, type, class), keeping order of
/// first appearance; RRSIGs group under (owner, RRSIG) and are matched to
/// what they cover by the caller.
pub fn rrsets(records: &[Record]) -> Vec<Vec<Record>> {
    let mut out: Vec<Vec<Record>> = Vec::new();
    for r in records {
        if let Some(set) = out
            .iter_mut()
            .find(|s| s[0].rtype == r.rtype && s[0].class == r.class && s[0].name == r.name && s[0].covered() == r.covered())
        {
            if !set.iter().any(|x| x.data == r.data) {
                set.push(r.clone());
            }
        } else {
            out.push(vec![r.clone()]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_compression() {
        let n = Name::parse("www.example.com").unwrap();
        let mut m = Message::query(0x1234, &n, ty::A, true, false, true);
        m.qr = true;
        let mut mx = vec![0, 10];
        mx.extend_from_slice(Name::parse("mail.example.com").unwrap().wire());
        m.answers.push(Record::new(n.clone(), ty::A, 300, vec![192, 0, 2, 1]));
        m.answers.push(Record::new(n.clone(), ty::MX, 300, mx.clone()));
        m.add_ede(ede::DNSSEC_BOGUS, "test");
        let b = m.to_bytes(65535);
        let p = Message::parse(&b).unwrap();
        assert_eq!(p.id, 0x1234);
        assert!(p.qr && p.rd && p.dnssec_ok());
        assert_eq!(p.answers.len(), 2);
        assert_eq!(p.answers[1].data, mx, "MX rdata decompressed back to the original");
        assert_eq!(p.edns_option(OPT_EDE).unwrap()[..2], ede::DNSSEC_BOGUS.to_be_bytes());
        // compression made it smaller than the sum of names
        assert!(b.len() < 12 + 17 + 4 + 2 * (17 + 10) + 4 + 18 + 30);
    }

    #[test]
    fn truncation_sets_tc() {
        let n = Name::parse("big.example").unwrap();
        let mut m = Message::query(1, &n, ty::TXT, true, false, false);
        m.qr = true;
        for i in 0..20u8 {
            let mut d = vec![200];
            d.extend(std::iter::repeat(b'a' + (i % 26)).take(200));
            m.answers.push(Record::new(n.clone(), ty::TXT, 60, d));
        }
        let b = m.to_bytes(512);
        assert!(b.len() <= 512);
        let p = Message::parse(&b).unwrap();
        assert!(p.tc);
        assert!(p.answers.is_empty());
    }

    #[test]
    fn padding_blocks() {
        let n = Name::parse("example.org").unwrap();
        let mut m = Message::query(1, &n, ty::A, true, false, false);
        m.qr = true;
        let b = m.to_bytes_padded(65535, 468);
        assert_eq!(b.len() % 468, 0);
        assert!(Message::parse(&b).is_ok());
    }
}
