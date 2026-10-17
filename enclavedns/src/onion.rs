//! `.onion` names answered here and never sent to the DNS (RFC 7686).
//!
//! A v3 onion address is its service's Ed25519 public key: 56 base32
//! characters encoding key (32) || checksum (2) || version (1), where the
//! checksum is SHA3-256(".onion checksum" || key || version)[..2]. An
//! address that fails any of that, or whose key is not a usable curve
//! point, cannot name a service: NXDOMAIN, authoritatively. Subdomains
//! (`www.<address>.onion`) name the same service, as Tor treats them.
//!
//! A valid address answers:
//!   * TXT: `"v=onion3" "key=ed25519:<base64 key>"`, the service identity;
//!   * A/AAAA: the configured onion gateway (a Tor transparent proxy or
//!     onion-to-web gateway the operator runs), as addresses or as a CNAME
//!     `<address>.onion.<suffix>`; with no gateway configured, no data.
//! Reaching the service still takes Tor; this server never pretends
//! otherwise, and never leaks the name to anyone.

use crate::name::Name;
use crate::resolver::Answer;
use crate::wire::{ede, rcode, ty, Record};
use base64::Engine;
use sha3::{Digest, Sha3_256};
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Default)]
pub struct OnionConfig {
    pub a: Vec<Ipv4Addr>,
    pub aaaa: Vec<Ipv6Addr>,
    pub cname_suffix: Option<String>,
    pub ttl: u32,
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

#[derive(Debug, PartialEq, Eq)]
pub struct OnionV3 {
    pub address: String,
    pub key: [u8; 32],
}

/// Validate one 56-character service label.
pub fn parse_v3(label: &str) -> Result<OnionV3, String> {
    let l = label.to_ascii_lowercase();
    if l.len() == 16 {
        return Err("v2 onion addresses were retired by Tor in 2021".into());
    }
    if l.len() != 56 {
        return Err(format!("an onion v3 address is 56 characters, not {}", l.len()));
    }
    let raw = crate::dnssec::base32_decode(l.as_bytes(), B32).ok_or("not base32")?;
    if raw.len() != 35 {
        return Err("bad length after decoding".into());
    }
    let key: [u8; 32] = raw[..32].try_into().unwrap();
    let version = raw[34];
    if version != 3 {
        return Err(format!("version byte {version}, expected 3"));
    }
    let mut h = Sha3_256::new();
    h.update(b".onion checksum");
    h.update(key);
    h.update([version]);
    if h.finalize()[..2] != raw[32..34] {
        return Err("checksum does not match (a typo, or not an onion address)".into());
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&key).map_err(|_| "not a point on the Ed25519 curve")?;
    if vk.is_weak() {
        return Err("a small-order key cannot belong to a service".into());
    }
    Ok(OnionV3 { address: l, key })
}

/// The service label of a name under `onion.` (the one just left of it).
pub fn service_label(name: &Name) -> Option<String> {
    let labels = name.labels();
    if labels.len() < 2 {
        return None;
    }
    std::str::from_utf8(labels[labels.len() - 2]).ok().map(|s| s.to_string())
}

pub fn answer(cfg: &OnionConfig, qname: &Name, qtype: u16) -> Answer {
    let mut a = Answer::negative(rcode::NOERROR, cfg.ttl, "onion");
    a.aa = true;
    let Some(label) = service_label(qname) else {
        // `onion.` itself
        return a;
    };
    let svc = match parse_v3(&label) {
        Ok(s) => s,
        Err(e) => {
            a.rcode = rcode::NXDOMAIN;
            a.ede = Some((ede::INVALID_DATA, format!("invalid onion address: {e}")));
            return a;
        }
    };
    let txt = |parts: &[&str]| -> Vec<u8> {
        let mut d = Vec::new();
        for p in parts {
            d.push(p.len() as u8);
            d.extend_from_slice(p.as_bytes());
        }
        d
    };
    match qtype {
        ty::TXT => {
            let k = format!("key=ed25519:{}", base64::engine::general_purpose::STANDARD.encode(svc.key));
            a.answer.push(Record::new(qname.clone(), ty::TXT, cfg.ttl, txt(&["v=onion3", &k])));
        }
        ty::A | ty::AAAA => {
            if let Some(sfx) = &cfg.cname_suffix {
                if let Ok(t) = Name::parse(&format!("{}.{sfx}", qname.to_text())) {
                    a.answer.push(Record::new(qname.clone(), ty::CNAME, cfg.ttl, t.wire().to_vec()));
                }
            } else if qtype == ty::A {
                a.answer.extend(cfg.a.iter().map(|ip| Record::new(qname.clone(), ty::A, cfg.ttl, ip.octets().to_vec())));
            } else {
                a.answer.extend(cfg.aaaa.iter().map(|ip| Record::new(qname.clone(), ty::AAAA, cfg.ttl, ip.octets().to_vec())));
            }
        }
        _ => {}
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    // the Tor Project's own onion service
    const TOR: &str = "2gzyxa5ihm7nsggfxnu52rck2vv4rvmdlkiu3zzui5du4xyclen53wid";

    #[test]
    fn real_address_validates() {
        let s = parse_v3(TOR).unwrap();
        assert_eq!(s.address, TOR);
        assert!(parse_v3(&TOR.to_uppercase()).is_ok());
    }

    #[test]
    fn typos_and_junk_fail() {
        let mut t = TOR.as_bytes().to_vec();
        t[10] = if t[10] == b'a' { b'b' } else { b'a' };
        assert!(parse_v3(std::str::from_utf8(&t).unwrap()).unwrap_err().contains("checksum"));
        assert!(parse_v3("expyuzz4wqqyqhjn").unwrap_err().contains("v2"));
        assert!(parse_v3("hello").is_err());
    }

    #[test]
    fn answers() {
        let cfg = OnionConfig { aaaa: vec!["2001:db8::1".parse().unwrap()], ttl: 300, ..Default::default() };
        let q = Name::parse(&format!("www.{TOR}.onion")).unwrap();
        let a = answer(&cfg, &q, ty::AAAA);
        assert_eq!(a.rcode, rcode::NOERROR);
        assert_eq!(a.answer.len(), 1);
        let t = answer(&cfg, &q, ty::TXT);
        assert!(crate::rdata::to_text(ty::TXT, &t.answer[0].data).contains("key=ed25519:"));
        let bad = answer(&cfg, &Name::parse("nope.onion").unwrap(), ty::A);
        assert_eq!(bad.rcode, rcode::NXDOMAIN);
        let c = OnionConfig { cname_suffix: Some("onion.example".into()), ttl: 60, ..Default::default() };
        let x = answer(&c, &Name::parse(&format!("{TOR}.onion")).unwrap(), ty::A);
        assert_eq!(crate::rdata::to_text(ty::CNAME, &x.answer[0].data), format!("{TOR}.onion.onion.example."));
    }
}
