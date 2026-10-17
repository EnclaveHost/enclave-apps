//! DNSSEC primitives as pure functions: keys and key tags (RFC 4034), DS
//! digests, signature verification for every algorithm a validator must
//! support today (RFC 8624), canonical RRset form, online signing with
//! ECDSA P-256, and the NSEC (RFC 4035) and NSEC3 (RFC 5155) denial proofs.
//! The chain of trust that decides which keys to use lives in the resolver.

use crate::name::Name;
use crate::wire::{ede, ty, Record};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::cmp::Ordering;

/// The root zone's key-signing keys, as DS records (IANA root-anchors.xml):
/// KSK-2017 and KSK-2024. Either one validates the root DNSKEY RRset.
pub const ROOT_ANCHORS: &[(u16, u8, u8, &str)] = &[
    (20326, 8, 2, "E06D44B80B8F1D39A95C0B0D7C65D08458E880409BBC683457104237C7F8EC8D"),
    (38696, 8, 2, "683D2D0ACB8C9B712A1948B27F741219298D0A450D612C483AF444A4C0FB2B16"),
];

pub fn root_anchor_ds() -> Vec<Vec<u8>> {
    ROOT_ANCHORS
        .iter()
        .map(|(tag, alg, dt, hex)| {
            let mut v = tag.to_be_bytes().to_vec();
            v.push(*alg);
            v.push(*dt);
            v.extend((0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()));
            v
        })
        .collect()
}

/// RFC 9276: past this many extra NSEC3 iterations a proof is not worth
/// computing; such answers are treated as insecure (EDE 27).
pub const NSEC3_MAX_ITERATIONS: u16 = 100;

#[derive(Clone, Debug)]
pub struct Dnskey {
    pub flags: u16,
    pub protocol: u8,
    pub alg: u8,
    pub key: Vec<u8>,
    pub tag: u16,
    pub rdata: Vec<u8>,
}

impl Dnskey {
    pub fn parse(rdata: &[u8]) -> Option<Dnskey> {
        if rdata.len() < 5 {
            return None;
        }
        Some(Dnskey {
            flags: u16::from_be_bytes([rdata[0], rdata[1]]),
            protocol: rdata[2],
            alg: rdata[3],
            key: rdata[4..].to_vec(),
            tag: key_tag(rdata),
            rdata: rdata.to_vec(),
        })
    }

    /// Usable for zone data: ZONE bit, protocol 3, not revoked (RFC 5011).
    pub fn usable(&self) -> bool {
        self.flags & 0x0100 != 0 && self.protocol == 3 && self.flags & 0x0080 == 0
    }
}

/// RFC 4034 Appendix B.
pub fn key_tag(rdata: &[u8]) -> u16 {
    let mut ac: u32 = 0;
    for (i, &b) in rdata.iter().enumerate() {
        ac += if i & 1 == 1 { b as u32 } else { (b as u32) << 8 };
    }
    ac += (ac >> 16) & 0xFFFF;
    (ac & 0xFFFF) as u16
}

pub fn alg_supported(alg: u8) -> bool {
    matches!(alg, 5 | 7 | 8 | 10 | 13 | 14 | 15)
}

pub fn digest_supported(dt: u8) -> bool {
    matches!(dt, 1 | 2 | 4)
}

/// The digest a DS record of `digest_type` would carry for this key.
pub fn ds_digest(owner: &Name, dnskey_rdata: &[u8], digest_type: u8) -> Option<Vec<u8>> {
    let mut input = owner.to_lowercase().wire().to_vec();
    input.extend_from_slice(dnskey_rdata);
    Some(match digest_type {
        1 => Sha1::digest(&input).to_vec(),
        2 => Sha256::digest(&input).to_vec(),
        4 => Sha384::digest(&input).to_vec(),
        _ => return None,
    })
}

/// Does DS rdata `ds` name this key? None when the DS digest type or key
/// algorithm is one this validator does not implement.
pub fn ds_matches(owner: &Name, key: &Dnskey, ds: &[u8]) -> Option<bool> {
    if ds.len() < 5 {
        return Some(false);
    }
    let (tag, alg, dt) = (u16::from_be_bytes([ds[0], ds[1]]), ds[2], ds[3]);
    if !digest_supported(dt) || !alg_supported(alg) {
        return None;
    }
    if tag != key.tag || alg != key.alg {
        return Some(false);
    }
    Some(ds_digest(owner, &key.rdata, dt).is_some_and(|d| d == ds[4..]))
}

/// Verify one signature. Err for an algorithm this build cannot check.
pub fn verify(alg: u8, key: &[u8], data: &[u8], sig: &[u8]) -> Result<bool, ()> {
    match alg {
        5 | 7 | 8 | 10 => Ok(verify_rsa(alg, key, data, sig).unwrap_or(false)),
        13 => {
            use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
            if key.len() != 64 || sig.len() != 64 {
                return Ok(false);
            }
            let mut sec1 = vec![4u8];
            sec1.extend_from_slice(key);
            let (Ok(vk), Ok(s)) = (VerifyingKey::from_sec1_bytes(&sec1), Signature::from_slice(sig)) else {
                return Ok(false);
            };
            Ok(vk.verify(data, &s).is_ok())
        }
        14 => {
            use p384::ecdsa::{signature::Verifier, Signature, VerifyingKey};
            if key.len() != 96 || sig.len() != 96 {
                return Ok(false);
            }
            let mut sec1 = vec![4u8];
            sec1.extend_from_slice(key);
            let (Ok(vk), Ok(s)) = (VerifyingKey::from_sec1_bytes(&sec1), Signature::from_slice(sig)) else {
                return Ok(false);
            };
            Ok(vk.verify(data, &s).is_ok())
        }
        15 => {
            use ed25519_dalek::{Signature, Verifier, VerifyingKey};
            let (Ok(k), Ok(s)) = (<[u8; 32]>::try_from(key), <[u8; 64]>::try_from(sig)) else {
                return Ok(false);
            };
            let Ok(vk) = VerifyingKey::from_bytes(&k) else {
                return Ok(false);
            };
            Ok(vk.verify(data, &Signature::from_bytes(&s)).is_ok())
        }
        _ => Err(()),
    }
}

/// RFC 3110 key layout: exponent length (1 octet, or 0 + 2 octets),
/// exponent, modulus.
fn verify_rsa(alg: u8, key: &[u8], data: &[u8], sig: &[u8]) -> Option<bool> {
    use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
    let (elen, off) = if *key.first()? == 0 {
        (u16::from_be_bytes([*key.get(1)?, *key.get(2)?]) as usize, 3)
    } else {
        (key[0] as usize, 1)
    };
    let e = BigUint::from_bytes_be(key.get(off..off + elen)?);
    let n_bytes = key.get(off + elen..)?;
    if n_bytes.is_empty() || n_bytes.len() > 512 {
        return Some(false);
    }
    let n = BigUint::from_bytes_be(n_bytes);
    let pk = RsaPublicKey::new_with_max_size(n, e, 4096).ok()?;
    // a signature shorter than the modulus is left-padded (RFC 3110 §3)
    let mut s = sig.to_vec();
    while s.len() < n_bytes.len() {
        s.insert(0, 0);
    }
    let ok = match alg {
        5 | 7 => pk.verify(Pkcs1v15Sign::new::<Sha1>(), &Sha1::digest(data), &s),
        8 => pk.verify(Pkcs1v15Sign::new::<Sha256>(), &Sha256::digest(data), &s),
        10 => pk.verify(Pkcs1v15Sign::new::<Sha512>(), &Sha512::digest(data), &s),
        _ => return None,
    };
    Some(ok.is_ok())
}

#[derive(Clone, Debug)]
pub struct Rrsig {
    pub type_covered: u16,
    pub alg: u8,
    pub labels: u8,
    pub orig_ttl: u32,
    pub expiration: u32,
    pub inception: u32,
    pub key_tag: u16,
    pub signer: Name,
    pub signature: Vec<u8>,
}

impl Rrsig {
    pub fn parse(d: &[u8]) -> Option<Rrsig> {
        if d.len() < 19 {
            return None;
        }
        let mut p = 18;
        let signer = Name::read(d, &mut p).ok()?;
        Some(Rrsig {
            type_covered: u16::from_be_bytes([d[0], d[1]]),
            alg: d[2],
            labels: d[3],
            orig_ttl: u32::from_be_bytes(d[4..8].try_into().ok()?),
            expiration: u32::from_be_bytes(d[8..12].try_into().ok()?),
            inception: u32::from_be_bytes(d[12..16].try_into().ok()?),
            key_tag: u16::from_be_bytes([d[16], d[17]]),
            signer,
            signature: d[p..].to_vec(),
        })
    }

    /// The RRSIG rdata minus the signature, signer canonicalized.
    fn head(&self) -> Vec<u8> {
        let mut h = Vec::with_capacity(18 + self.signer.wire().len());
        h.extend_from_slice(&self.type_covered.to_be_bytes());
        h.push(self.alg);
        h.push(self.labels);
        h.extend_from_slice(&self.orig_ttl.to_be_bytes());
        h.extend_from_slice(&self.expiration.to_be_bytes());
        h.extend_from_slice(&self.inception.to_be_bytes());
        h.extend_from_slice(&self.key_tag.to_be_bytes());
        h.extend_from_slice(self.signer.to_lowercase().wire());
        h
    }

    pub fn to_rdata(&self) -> Vec<u8> {
        let mut h = self.head();
        // the stored signer keeps its case in the record itself
        let start = 18;
        h.truncate(start);
        h.extend_from_slice(self.signer.wire());
        h.extend_from_slice(&self.signature);
        h
    }
}

/// Owner labels for RRSIG purposes: a leading `*` does not count.
pub fn rrsig_label_count(owner: &Name) -> usize {
    let n = owner.label_count();
    if owner.is_wildcard() {
        n - 1
    } else {
        n
    }
}

/// Types whose embedded names are lowercased in canonical form
/// (RFC 4034 §6.2 as amended by RFC 6840 §5.1: not NSEC, not HINFO).
fn canonical_rdata(rtype: u16, data: &[u8]) -> Vec<u8> {
    let (prefix, names, char_strings): (usize, usize, usize) = match rtype {
        ty::NS | ty::CNAME | ty::PTR | ty::DNAME | 3 | 4 | 7 | 8 | 9 => (0, 1, 0),
        ty::SOA | 14 | ty::RP => (0, 2, 0),
        ty::MX | ty::AFSDB | 21 | ty::KX => (2, 1, 0),
        26 => (2, 2, 0),
        ty::SRV => (6, 1, 0),
        ty::NAPTR => (4, 1, 3),
        ty::RRSIG | ty::SIG => (18, 1, 0),
        _ => return data.to_vec(),
    };
    let mut out = data.to_vec();
    let mut p = prefix;
    for _ in 0..char_strings {
        match out.get(p) {
            Some(&l) => p += 1 + l as usize,
            None => return data.to_vec(),
        }
    }
    for _ in 0..names {
        let start = p;
        match Name::read(data, &mut p) {
            Ok(_) => out[start..p].make_ascii_lowercase(),
            Err(_) => return data.to_vec(),
        }
    }
    out
}

/// The octets an RRSIG signs (RFC 4034 §3.1.8.1, §6).
pub fn signed_data(sig: &Rrsig, owner: &Name, rtype: u16, class: u16, rdatas: &[Vec<u8>]) -> Vec<u8> {
    let mut out = sig.head();
    let owner_lc = owner.to_lowercase();
    let owner_wire = if (sig.labels as usize) < rrsig_label_count(owner) {
        // wildcard expansion: sign as "*." + the rightmost `labels` labels
        owner_lc.suffix(sig.labels as usize).prepend(b"*").map(|n| n.wire().to_vec()).unwrap_or_default()
    } else {
        owner_lc.wire().to_vec()
    };
    let mut canon: Vec<Vec<u8>> = rdatas.iter().map(|d| canonical_rdata(rtype, d)).collect();
    canon.sort();
    canon.dedup();
    for d in canon {
        out.extend_from_slice(&owner_wire);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&class.to_be_bytes());
        out.extend_from_slice(&sig.orig_ttl.to_be_bytes());
        out.extend_from_slice(&(d.len() as u16).to_be_bytes());
        out.extend_from_slice(&d);
    }
    out
}

/// RFC 1982 serial arithmetic on signature times.
pub fn time_after_eq(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

#[derive(Clone, Debug)]
pub struct Verified {
    /// A cap on the RRset's TTL: the original TTL, and the time left before
    /// the signature expires.
    pub ttl_cap: u32,
    /// The RRSIG labels field; fewer than the owner's labels = wildcard.
    pub labels: u8,
    pub wildcard: bool,
}

pub type Failure = (u16, String);

/// Verify an RRset against a zone's validated keys: any one good RRSIG
/// from `zone` suffices.
#[allow(clippy::too_many_arguments)]
pub fn verify_rrset(
    owner: &Name,
    rtype: u16,
    class: u16,
    rdatas: &[Vec<u8>],
    sigs: &[Rrsig],
    keys: &[Dnskey],
    zone: &Name,
    now: u32,
) -> Result<Verified, Failure> {
    let mut why: Failure = (ede::RRSIGS_MISSING, format!("no RRSIG for {owner} {}", ty::name(rtype)));
    let owner_labels = rrsig_label_count(owner);
    for s in sigs {
        if s.type_covered != rtype {
            continue;
        }
        if s.signer != *zone || !owner.is_subdomain_of(&s.signer) {
            why = (ede::DNSSEC_BOGUS, format!("RRSIG for {owner} {} signed by {}, expected {zone}", ty::name(rtype), s.signer));
            continue;
        }
        if !alg_supported(s.alg) {
            why = (ede::UNSUPPORTED_DNSKEY_ALG, format!("algorithm {} not supported", s.alg));
            continue;
        }
        if s.labels as usize > owner_labels {
            why = (ede::DNSSEC_BOGUS, "RRSIG labels exceed the owner's".into());
            continue;
        }
        if !time_after_eq(s.expiration, now) {
            why = (ede::SIGNATURE_EXPIRED, format!("RRSIG for {owner} {} expired {}", ty::name(rtype), sig_time_text(s.expiration)));
            continue;
        }
        if !time_after_eq(now, s.inception) {
            why = (ede::SIGNATURE_NOT_YET_VALID, format!("RRSIG for {owner} {} not valid until {}", ty::name(rtype), sig_time_text(s.inception)));
            continue;
        }
        let candidates: Vec<&Dnskey> = keys.iter().filter(|k| k.tag == s.key_tag && k.alg == s.alg && k.usable()).collect();
        if candidates.is_empty() {
            why = (ede::DNSKEY_MISSING, format!("no {zone} DNSKEY with tag {} for {owner} {}", s.key_tag, ty::name(rtype)));
            continue;
        }
        let data = signed_data(s, owner, rtype, class, rdatas);
        for k in candidates {
            if verify(s.alg, &k.key, &data, &s.signature) == Ok(true) {
                let left = s.expiration.wrapping_sub(now);
                return Ok(Verified {
                    ttl_cap: s.orig_ttl.min(left),
                    labels: s.labels,
                    wildcard: (s.labels as usize) < owner_labels,
                });
            }
        }
        why = (ede::DNSSEC_BOGUS, format!("RRSIG for {owner} {} does not verify (key {})", ty::name(rtype), s.key_tag));
    }
    Err(why)
}

// ---------------------------------------------------------------- signing --

/// An online ECDSA P-256 signer (algorithm 13) for a hosted zone.
pub struct ZoneSigner {
    pub zone: Name,
    key: p256::ecdsa::SigningKey,
    pub dnskey_rdata: Vec<u8>,
    pub tag: u16,
}

impl ZoneSigner {
    /// The private scalar is HMAC-SHA256(seed, "enclavedns zone key v1" ||
    /// zone || counter), retried on the (negligible) out-of-range case, so
    /// the same seed always gives a zone the same key and the DS at the
    /// parent survives restarts.
    pub fn from_seed(zone: &Name, seed: &[u8]) -> ZoneSigner {
        use hmac::{Hmac, Mac};
        let mut ctr = 0u8;
        let key = loop {
            let mut m = <Hmac<Sha256> as Mac>::new_from_slice(seed).unwrap();
            m.update(b"enclavedns zone key v1");
            m.update(zone.to_lowercase().wire());
            m.update(&[ctr]);
            let b = m.finalize().into_bytes();
            if let Ok(k) = p256::ecdsa::SigningKey::from_bytes(&b) {
                break k;
            }
            ctr += 1;
        };
        let point = key.verifying_key().to_encoded_point(false);
        let mut rdata = vec![0x01, 0x01, 3, 13]; // flags 257 (ZONE|SEP): one key signs everything
        rdata.extend_from_slice(&point.as_bytes()[1..]);
        let tag = key_tag(&rdata);
        ZoneSigner { zone: zone.clone(), key, dnskey_rdata: rdata, tag }
    }

    pub fn ds_rdata(&self) -> Vec<u8> {
        let mut v = self.tag.to_be_bytes().to_vec();
        v.push(13);
        v.push(2);
        v.extend_from_slice(&ds_digest(&self.zone, &self.dnskey_rdata, 2).unwrap());
        v
    }

    pub fn sign(&self, owner: &Name, rtype: u16, ttl: u32, rdatas: &[Vec<u8>], inception: u32, expiration: u32) -> Record {
        use p256::ecdsa::{signature::Signer, Signature};
        let mut s = Rrsig {
            type_covered: rtype,
            alg: 13,
            labels: rrsig_label_count(owner) as u8,
            orig_ttl: ttl,
            expiration,
            inception,
            key_tag: self.tag,
            signer: self.zone.clone(),
            signature: Vec::new(),
        };
        let data = signed_data(&s, owner, rtype, crate::wire::CLASS_IN, rdatas);
        let sig: Signature = self.key.sign(&data);
        s.signature = sig.to_bytes().to_vec();
        Record::new(owner.clone(), ty::RRSIG, ttl, s.to_rdata())
    }
}

// ------------------------------------------------------- type bitmaps ------

pub fn bitmap_types(b: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 2 <= b.len() {
        let (win, len) = (b[p] as u16, b[p + 1] as usize);
        let Some(bits) = b.get(p + 2..p + 2 + len) else { break };
        for (i, byte) in bits.iter().enumerate() {
            for bit in 0..8 {
                if byte & (0x80 >> bit) != 0 {
                    out.push(win * 256 + (i * 8 + bit) as u16);
                }
            }
        }
        p += 2 + len;
    }
    out
}

pub fn bitmap_has(b: &[u8], t: u16) -> bool {
    let (win, idx) = ((t >> 8) as u8, (t & 0xFF) as usize);
    let mut p = 0;
    while p + 2 <= b.len() {
        let len = b[p + 1] as usize;
        if b[p] == win {
            return b.get(p + 2 + idx / 8).is_some_and(|x| x & (0x80 >> (idx % 8)) != 0);
        }
        p += 2 + len;
    }
    false
}

pub fn make_bitmap(types: &[u16]) -> Vec<u8> {
    let mut ts = types.to_vec();
    ts.sort();
    ts.dedup();
    let mut out = Vec::new();
    let mut i = 0;
    while i < ts.len() {
        let win = ts[i] >> 8;
        let mut bits = [0u8; 32];
        let mut max = 0;
        while i < ts.len() && ts[i] >> 8 == win {
            let idx = (ts[i] & 0xFF) as usize;
            bits[idx / 8] |= 0x80 >> (idx % 8);
            max = max.max(idx / 8 + 1);
            i += 1;
        }
        out.push(win as u8);
        out.push(max as u8);
        out.extend_from_slice(&bits[..max]);
    }
    out
}

// ---------------------------------------------------------------- base32 ---

const B32HEX: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";

pub fn base32hex_encode(b: &[u8]) -> String {
    base32_encode(b, B32HEX)
}

pub fn base32_encode(b: &[u8], alphabet: &[u8; 32]) -> String {
    let mut out = String::new();
    let mut buf: u64 = 0;
    let mut bits = 0;
    for &x in b {
        buf = (buf << 8) | x as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(alphabet[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(alphabet[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &[u8], alphabet: &[u8; 32]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf: u64 = 0;
    let mut bits = 0;
    for &c in s {
        let c = c.to_ascii_lowercase();
        let v = alphabet.iter().position(|&a| a == c)? as u64;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    // leftover bits must be zero padding
    if bits >= 5 || (buf & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

pub fn base32hex_decode(s: &[u8]) -> Option<Vec<u8>> {
    base32_decode(s, B32HEX)
}

/// YYYYMMDDHHmmSS (RRSIG presentation).
pub fn sig_time_text(t: u32) -> String {
    let secs = t as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil_from_days (H. Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ---------------------------------------------------------- denial proofs --

#[derive(Clone, Debug)]
pub struct Nsec {
    pub owner: Name,
    pub next: Name,
    pub bitmap: Vec<u8>,
}

impl Nsec {
    pub fn parse(owner: &Name, d: &[u8]) -> Option<Nsec> {
        let mut p = 0;
        let next = Name::read(d, &mut p).ok()?;
        Some(Nsec { owner: owner.clone(), next, bitmap: d[p..].to_vec() })
    }

    fn has(&self, t: u16) -> bool {
        bitmap_has(&self.bitmap, t)
    }

    /// owner < name < next in canonical order (with the wrap at the end of
    /// the chain), and the NSEC is not a delegation's or DNAME's that cannot
    /// speak for names beneath it.
    pub fn covers(&self, name: &Name) -> bool {
        if *name == self.owner {
            return false;
        }
        if name.is_subdomain_of(&self.owner) && ((self.has(ty::NS) && !self.has(ty::SOA)) || self.has(ty::DNAME)) {
            return false;
        }
        let after_owner = self.owner.canonical_cmp(name) == Ordering::Less;
        match self.owner.canonical_cmp(&self.next) {
            Ordering::Less => after_owner && name.canonical_cmp(&self.next) == Ordering::Less,
            // last NSEC in the chain: next wraps to the apex
            _ => after_owner || name.canonical_cmp(&self.next) == Ordering::Less,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Nsec3 {
    pub hash_alg: u8,
    pub flags: u8,
    pub iterations: u16,
    pub salt: Vec<u8>,
    pub next_hash: Vec<u8>,
    pub bitmap: Vec<u8>,
    pub owner_hash: Vec<u8>,
    pub zone: Name,
}

impl Nsec3 {
    pub fn parse_rdata(d: &[u8]) -> Option<Nsec3> {
        let sl = *d.get(4)? as usize;
        let salt = d.get(5..5 + sl)?.to_vec();
        let hl = *d.get(5 + sl)? as usize;
        let next_hash = d.get(6 + sl..6 + sl + hl)?.to_vec();
        Some(Nsec3 {
            hash_alg: d[0],
            flags: d[1],
            iterations: u16::from_be_bytes([d[2], d[3]]),
            salt,
            next_hash,
            bitmap: d.get(6 + sl + hl..)?.to_vec(),
            owner_hash: Vec::new(),
            zone: Name::root(),
        })
    }

    pub fn parse(owner: &Name, d: &[u8]) -> Option<Nsec3> {
        let mut n = Nsec3::parse_rdata(d)?;
        n.owner_hash = base32hex_decode(owner.first_label())?;
        n.zone = owner.parent()?;
        Some(n)
    }

    pub fn opt_out(&self) -> bool {
        self.flags & 1 != 0
    }

    fn has(&self, t: u16) -> bool {
        bitmap_has(&self.bitmap, t)
    }

    fn matches(&self, h: &[u8]) -> bool {
        self.owner_hash == h
    }

    fn covers(&self, h: &[u8]) -> bool {
        if self.owner_hash.as_slice() < self.next_hash.as_slice() {
            self.owner_hash.as_slice() < h && h < self.next_hash.as_slice()
        } else {
            h > self.owner_hash.as_slice() || h < self.next_hash.as_slice()
        }
    }
}

pub fn nsec3_hash(name: &Name, salt: &[u8], iterations: u16) -> Vec<u8> {
    let mut h = Sha1::new();
    h.update(name.to_lowercase().wire());
    h.update(salt);
    let mut d = h.finalize();
    for _ in 0..iterations {
        let mut h = Sha1::new();
        h.update(d);
        h.update(salt);
        d = h.finalize();
    }
    d.to_vec()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denial {
    /// The nonexistence is proven.
    Secure,
    /// The proof shows an unsigned delegation (NSEC3 opt-out, or a DS
    /// NODATA at a delegation), or NSEC3 too costly to check: data under
    /// it is insecure, not bogus.
    Insecure(u16, String),
    Bogus(u16, String),
}

fn bogus(why: impl Into<String>) -> Denial {
    Denial::Bogus(ede::NSEC_MISSING, why.into())
}

/// What a negative answer must prove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    NxDomain,
    NoData,
    /// A positive answer synthesized from a wildcard: prove the next
    /// closer name (one label below the source of synthesis) is absent.
    WildcardAnswer { labels: u8 },
}

/// Check the NSEC or NSEC3 records in a (validated) authority section prove
/// `claim` for (qname, qtype). Records must already be signature-checked.
pub fn prove(qname: &Name, qtype: u16, claim: Claim, nsecs: &[Nsec], nsec3s: &[Nsec3]) -> Denial {
    if !nsecs.is_empty() {
        let r = prove_nsec(qname, qtype, claim, nsecs);
        if r == Denial::Secure || nsec3s.is_empty() {
            return r;
        }
    }
    if !nsec3s.is_empty() {
        return prove_nsec3(qname, qtype, claim, nsec3s);
    }
    bogus(format!("no NSEC/NSEC3 records prove the {claim:?} for {qname} {}", ty::name(qtype)))
}

fn closest_encloser_nsec(qname: &Name, n: &Nsec) -> Name {
    let a = qname.common_ancestor(&n.owner);
    let b = qname.common_ancestor(&n.next);
    if a.label_count() >= b.label_count() {
        a
    } else {
        b
    }
}

fn prove_nsec(qname: &Name, qtype: u16, claim: Claim, nsecs: &[Nsec]) -> Denial {
    match claim {
        Claim::NxDomain => {
            let Some(c) = nsecs.iter().find(|n| n.covers(qname)) else {
                return bogus(format!("no NSEC covers {qname}"));
            };
            let ce = closest_encloser_nsec(qname, c);
            let Ok(wc) = ce.prepend(b"*") else { return bogus("bad closest encloser") };
            if nsecs.iter().any(|n| n.owner == wc) {
                return bogus(format!("{wc} exists, so {qname} should have been synthesized"));
            }
            if !nsecs.iter().any(|n| n.covers(&wc)) {
                return bogus(format!("no NSEC denies the wildcard {wc}"));
            }
            Denial::Secure
        }
        Claim::NoData => {
            if let Some(m) = nsecs.iter().find(|n| n.owner == *qname) {
                if m.has(qtype) || (qtype != ty::CNAME && m.has(ty::CNAME)) {
                    return bogus(format!("NSEC at {qname} lists {}", ty::name(qtype)));
                }
                if qtype == ty::DS {
                    if m.has(ty::SOA) && !qname.is_root() {
                        return bogus("DS denied by the child zone's NSEC");
                    }
                    if m.has(ty::NS) {
                        return Denial::Insecure(ede::OTHER, format!("{qname} is an unsigned delegation"));
                    }
                } else if m.has(ty::NS) && !m.has(ty::SOA) {
                    return bogus("NODATA from a delegation's parent-side NSEC");
                }
                return Denial::Secure;
            }
            // empty non-terminal: an NSEC whose next name lies beneath qname
            if nsecs.iter().any(|n| {
                n.owner.canonical_cmp(qname) == Ordering::Less
                    && n.next.is_subdomain_of(qname)
                    && n.next != *qname
            }) {
                return Denial::Secure;
            }
            // wildcard NODATA: qname absent, the wildcard exists without qtype
            if let Some(c) = nsecs.iter().find(|n| n.covers(qname)) {
                let ce = closest_encloser_nsec(qname, c);
                if let Ok(wc) = ce.prepend(b"*") {
                    if let Some(w) = nsecs.iter().find(|n| n.owner == wc) {
                        if !w.has(qtype) && !w.has(ty::CNAME) {
                            return Denial::Secure;
                        }
                    }
                }
            }
            bogus(format!("no NSEC proves {qname} has no {}", ty::name(qtype)))
        }
        Claim::WildcardAnswer { labels } => {
            let n = labels as usize;
            if qname.label_count() <= n {
                return bogus("wildcard labels do not fit the name");
            }
            let next_closer = qname.suffix(n + 1);
            if nsecs.iter().any(|x| x.covers(&next_closer) || (x.covers(qname) && closest_encloser_nsec(qname, x).label_count() == n)) {
                Denial::Secure
            } else {
                bogus(format!("no NSEC proves {qname} was not there before wildcard expansion"))
            }
        }
    }
}

/// RFC 5155 §8.3 closest encloser proof. Returns (closest encloser, the
/// NSEC3 covering the next closer name).
fn closest_encloser_nsec3<'a>(
    qname: &Name,
    zone: &Name,
    hashed: &dyn Fn(&Name) -> Vec<u8>,
    set: &'a [Nsec3],
) -> Option<(Name, &'a Nsec3)> {
    let mut cand = qname.clone();
    let mut next_closer: Option<Name> = None;
    loop {
        let h = hashed(&cand);
        if let Some(m) = set.iter().find(|n| n.matches(&h)) {
            // a delegation or DNAME cannot be the closest encloser
            let deleg = m.has(ty::NS) && !m.has(ty::SOA);
            if !deleg && !m.has(ty::DNAME) {
                let nc = next_closer?;
                let hn = hashed(&nc);
                let cov = set.iter().find(|n| n.covers(&hn))?;
                return Some((cand, cov));
            }
            return None;
        }
        if cand == *zone {
            return None;
        }
        next_closer = Some(cand.clone());
        cand = cand.parent()?;
    }
}

fn prove_nsec3(qname: &Name, qtype: u16, claim: Claim, all: &[Nsec3]) -> Denial {
    // one parameter set, known hash algorithm, for the zone the first record names
    let usable: Vec<Nsec3> = all.iter().filter(|n| n.hash_alg == 1).cloned().collect();
    let Some(first) = usable.first().cloned() else {
        return bogus("NSEC3 with an unknown hash algorithm only");
    };
    let zone = first.zone.clone();
    let set: Vec<Nsec3> = usable
        .into_iter()
        .filter(|n| n.zone == zone && n.iterations == first.iterations && n.salt == first.salt)
        .collect();
    if !qname.is_subdomain_of(&zone) {
        return bogus(format!("NSEC3 from {zone} cannot speak for {qname}"));
    }
    if first.iterations > NSEC3_MAX_ITERATIONS {
        return Denial::Insecure(
            ede::UNSUPPORTED_NSEC3_ITERATIONS,
            format!("{zone} uses {} NSEC3 iterations (RFC 9276)", first.iterations),
        );
    }
    let cache = std::cell::RefCell::new(std::collections::HashMap::<Name, Vec<u8>>::new());
    let hashed = |n: &Name| -> Vec<u8> {
        if let Some(h) = cache.borrow().get(n) {
            return h.clone();
        }
        let h = nsec3_hash(n, &first.salt, first.iterations);
        cache.borrow_mut().insert(n.clone(), h.clone());
        h
    };
    match claim {
        Claim::NxDomain => {
            let Some((ce, _cov)) = closest_encloser_nsec3(qname, &zone, &hashed, &set) else {
                return bogus(format!("no NSEC3 closest-encloser proof for {qname}"));
            };
            let Ok(wc) = ce.prepend(b"*") else { return bogus("bad closest encloser") };
            let hw = hashed(&wc);
            if set.iter().any(|n| n.covers(&hw)) {
                Denial::Secure
            } else {
                bogus(format!("no NSEC3 denies the wildcard {wc}"))
            }
        }
        Claim::NoData => {
            let hq = hashed(qname);
            if let Some(m) = set.iter().find(|n| n.matches(&hq)) {
                if m.has(qtype) || (qtype != ty::CNAME && m.has(ty::CNAME)) {
                    return bogus(format!("NSEC3 for {qname} lists {}", ty::name(qtype)));
                }
                if qtype == ty::DS {
                    if m.has(ty::SOA) && !qname.is_root() {
                        return bogus("DS denied by the child zone's NSEC3");
                    }
                    if m.has(ty::NS) {
                        return Denial::Insecure(ede::OTHER, format!("{qname} is an unsigned delegation"));
                    }
                } else if m.has(ty::NS) && !m.has(ty::SOA) {
                    return bogus("NODATA from a delegation's parent-side NSEC3");
                }
                return Denial::Secure;
            }
            let Some((ce, cov)) = closest_encloser_nsec3(qname, &zone, &hashed, &set) else {
                return bogus(format!("no NSEC3 proves {qname} has no {}", ty::name(qtype)));
            };
            if qtype == ty::DS && cov.opt_out() {
                return Denial::Insecure(ede::OTHER, format!("{qname} lies in an NSEC3 opt-out span"));
            }
            // wildcard NODATA
            if let Ok(wc) = ce.prepend(b"*") {
                let hw = hashed(&wc);
                if let Some(w) = set.iter().find(|n| n.matches(&hw)) {
                    if !w.has(qtype) && !w.has(ty::CNAME) {
                        return Denial::Secure;
                    }
                }
            }
            bogus(format!("no NSEC3 proves {qname} has no {}", ty::name(qtype)))
        }
        Claim::WildcardAnswer { labels } => {
            let n = labels as usize;
            if qname.label_count() <= n {
                return bogus("wildcard labels do not fit the name");
            }
            let hn = hashed(&qname.suffix(n + 1));
            if set.iter().any(|x| x.covers(&hn)) {
                Denial::Secure
            } else {
                bogus(format!("no NSEC3 proves {qname} was not there before wildcard expansion"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    #[test]
    fn key_tag_rfc4034_example() {
        // RFC 4034 §5.4 example DNSKEY: tag 60485 for dskey.example.com
        use base64::Engine;
        let key = base64::engine::general_purpose::STANDARD
            .decode("AQOeiiR0GOMYkDshWoSKz9XzfwJr1AYtsmx3TGkJaNXVbfi/2pHm822aJ5iI9BMzNXxeYCmZDRD99WYwYqUSdjMmmAphXdvxegXd/M5+X7OrzKBaMbCVdFLUUh6DhweJBjEVv5f2wwjM9XzcnOf+EPbtG9DMBmADjFDc2w/rljwvFw==")
            .unwrap();
        let mut rdata = vec![1, 0, 3, 5];
        rdata.extend_from_slice(&key);
        assert_eq!(key_tag(&rdata), 60485);
        // and its DS (RFC 4034 §5.4): SHA-1 2BB183AF5F22588179A53B0A98631FAD1A292118
        let d = ds_digest(&n("dskey.example.com"), &rdata, 1).unwrap();
        assert_eq!(crate::rdata::to_hex(&d), "2bb183af5f22588179a53b0a98631fad1a292118");
    }

    #[test]
    fn nsec3_hash_rfc5155_appendix_a() {
        // salt aabbccdd, 12 iterations
        let salt = [0xaa, 0xbb, 0xcc, 0xdd];
        let h = nsec3_hash(&n("example"), &salt, 12);
        assert_eq!(base32hex_encode(&h), "0p9mhaveqvm6t7vbl5lop2u3t2rp3tom");
        let h = nsec3_hash(&n("a.example"), &salt, 12);
        assert_eq!(base32hex_encode(&h), "35mthgpgcu1qg68fab165klnsnk3dpvl");
        assert_eq!(base32hex_decode(b"35MTHGPGCU1QG68FAB165KLNSNK3DPVL").unwrap(), h);
    }

    #[test]
    fn bitmaps() {
        let b = make_bitmap(&[ty::A, ty::MX, ty::RRSIG, ty::NSEC, ty::CAA, 1234]);
        assert_eq!(bitmap_types(&b), vec![ty::A, ty::MX, ty::RRSIG, ty::NSEC, ty::CAA, 1234]);
        assert!(bitmap_has(&b, ty::CAA) && !bitmap_has(&b, ty::AAAA));
        // RFC 4034 §4.3 example bitmap: A MX RRSIG NSEC TYPE1234
        let mut ex = vec![0x00, 0x06, 0x40, 0x01, 0x00, 0x00, 0x00, 0x03, 0x04, 0x1b];
        ex.extend_from_slice(&[0u8; 26]);
        ex.push(0x20);
        assert_eq!(make_bitmap(&[ty::A, ty::MX, ty::RRSIG, ty::NSEC, 1234]), ex);
    }

    #[test]
    fn sign_then_verify_p256() {
        let zone = n("example.test");
        let s = ZoneSigner::from_seed(&zone, b"seed-seed-seed-seed-seed-seed-32");
        let s2 = ZoneSigner::from_seed(&zone, b"seed-seed-seed-seed-seed-seed-32");
        assert_eq!(s.dnskey_rdata, s2.dnskey_rdata, "same seed, same key");
        let owner = n("WWW.example.test");
        let rd = vec![vec![192, 0, 2, 1], vec![192, 0, 2, 2]];
        let now = 1_800_000_000;
        let sig = s.sign(&owner, ty::A, 300, &rd, now - 3600, now + 86400);
        let rr = Rrsig::parse(&sig.data).unwrap();
        let key = Dnskey::parse(&s.dnskey_rdata).unwrap();
        // records in a different order and case still verify
        let rd2 = vec![rd[1].clone(), rd[0].clone()];
        let v = verify_rrset(&n("www.EXAMPLE.test"), ty::A, 1, &rd2, &[rr.clone()], &[key.clone()], &zone, now).unwrap();
        assert!(!v.wildcard);
        // tampered data fails
        let bad = vec![vec![192, 0, 2, 9]];
        assert!(verify_rrset(&owner, ty::A, 1, &bad, &[rr.clone()], &[key.clone()], &zone, now).is_err());
        // expired fails with EDE 7
        let e = verify_rrset(&owner, ty::A, 1, &rd, &[rr], &[key.clone()], &zone, now + 90000).unwrap_err();
        assert_eq!(e.0, ede::SIGNATURE_EXPIRED);
        // the DS names the key
        assert_eq!(ds_matches(&zone, &key, &s.ds_rdata()), Some(true));
    }

    #[test]
    fn nsec_proofs() {
        let types = |t: &[u16]| make_bitmap(t);
        let nsecs = vec![
            Nsec { owner: n("example"), next: n("a.example"), bitmap: types(&[ty::NS, ty::SOA, ty::RRSIG, ty::NSEC, ty::DNSKEY]) },
            Nsec { owner: n("a.example"), next: n("ai.example"), bitmap: types(&[ty::NS, ty::DS, ty::RRSIG, ty::NSEC]) },
            Nsec { owner: n("ai.example"), next: n("b.example"), bitmap: types(&[ty::A, ty::RRSIG, ty::NSEC]) },
            Nsec { owner: n("b.example"), next: n("x.y.w.example"), bitmap: types(&[ty::NS, ty::RRSIG, ty::NSEC]) },
            Nsec { owner: n("x.y.w.example"), next: n("example"), bitmap: types(&[ty::MX, ty::RRSIG, ty::NSEC]) },
        ];
        // RFC 4035 B.2: ml.example NXDOMAIN, proven by b.example (covers) and example (covers *.example)
        assert_eq!(prove(&n("ml.example"), ty::A, Claim::NxDomain, &nsecs, &[]), Denial::Secure);
        // RFC 4035 B.3: NODATA for ai.example MX
        assert_eq!(prove(&n("ai.example"), ty::MX, Claim::NoData, &nsecs, &[]), Denial::Secure);
        assert!(matches!(prove(&n("ai.example"), ty::A, Claim::NoData, &nsecs, &[]), Denial::Bogus(..)));
        // y.w.example is an empty non-terminal
        assert_eq!(prove(&n("y.w.example"), ty::A, Claim::NoData, &nsecs, &[]), Denial::Secure);
        // under the a.example delegation the parent's NSEC proves nothing
        assert!(matches!(prove(&n("q.a.example"), ty::A, Claim::NxDomain, &nsecs, &[]), Denial::Bogus(..)));
        // DS NODATA at the unsigned delegation b.example = insecure
        assert!(matches!(prove(&n("b.example"), ty::DS, Claim::NoData, &nsecs, &[]), Denial::Insecure(..)));
    }

    #[test]
    fn nsec3_proofs_rfc5155() {
        // RFC 5155 Appendix B.1 (NXDOMAIN for a.c.x.w.example), zone example,
        // salt aabbccdd, 12 iterations; the three NSEC3 RRs of the response
        let salt = vec![0xaa, 0xbb, 0xcc, 0xdd];
        let mk = |owner: &str, next: &str, t: &[u16]| Nsec3 {
            hash_alg: 1,
            flags: 1,
            iterations: 12,
            salt: salt.clone(),
            next_hash: base32hex_decode(next.as_bytes()).unwrap(),
            bitmap: make_bitmap(t),
            owner_hash: base32hex_decode(owner.as_bytes()).unwrap(),
            zone: n("example"),
        };
        let set = vec![
            mk("0p9mhaveqvm6t7vbl5lop2u3t2rp3tom", "2t7b4g4vsa5smi47k61mv5bv1a22bojr", &[ty::NS, ty::SOA, ty::MX, ty::RRSIG, ty::DNSKEY, ty::NSEC3PARAM]),
            mk("b4um86eghhds6nea196smvmlo4ors995", "gjeqe526plbf1g8mklp59enfd789njgi", &[ty::MX, ty::RRSIG]),
            mk("35mthgpgcu1qg68fab165klnsnk3dpvl", "b4um86eghhds6nea196smvmlo4ors995", &[ty::NS, ty::DS, ty::RRSIG]),
        ];
        assert_eq!(prove(&n("a.c.x.w.example"), ty::A, Claim::NxDomain, &[], &set), Denial::Secure);
        // without the wildcard-covering record the proof fails
        assert!(matches!(prove(&n("a.c.x.w.example"), ty::A, Claim::NxDomain, &[], &set[..2]), Denial::Bogus(..)));
        // B.2: NODATA ns1.example MX, matching NSEC3 2t7b4g4vsa5smi47k61mv5bv1a22bojr
        let nd = vec![mk("2t7b4g4vsa5smi47k61mv5bv1a22bojr", "2vptu5timamqttgl4luu9kg21e0aor3s", &[ty::A, ty::RRSIG])];
        assert_eq!(prove(&n("ns1.example"), ty::MX, Claim::NoData, &[], &nd), Denial::Secure);
        assert!(matches!(prove(&n("ns1.example"), ty::A, Claim::NoData, &[], &nd), Denial::Bogus(..)));
    }

    #[test]
    fn root_anchor_parses() {
        assert_eq!(root_anchor_ds().len(), 2);
        assert_eq!(root_anchor_ds()[0].len(), 4 + 32);
    }

    #[test]
    fn times() {
        assert_eq!(sig_time_text(0), "19700101000000");
        assert_eq!(sig_time_text(1_700_000_000), "20231114221320");
        assert!(time_after_eq(5, 4) && !time_after_eq(4, 5));
        assert!(time_after_eq(1, u32::MAX)); // wraps
    }
}
