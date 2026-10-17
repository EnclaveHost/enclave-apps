//! Record data in presentation form: zone-file text to wire rdata (for
//! hosted zones) and back (for the JSON API and the status page).

use crate::name::Name;
use crate::wire::ty;
use base64::Engine;
use std::net::{Ipv4Addr, Ipv6Addr};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Split a zone-file line into tokens: whitespace separated, `"quoted
/// strings"` kept whole (escapes resolved later), `;` comments dropped,
/// parentheses ignored.
pub fn tokenize(line: &str) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_tok = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '\\' => {
                    cur.push('\\');
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '"' => {
                    out.push((std::mem::take(&mut cur), true));
                    quoted = false;
                    in_tok = false;
                }
                _ => cur.push(c),
            }
            continue;
        }
        match c {
            ';' => break,
            '"' => {
                if in_tok {
                    out.push((std::mem::take(&mut cur), false));
                }
                quoted = true;
                in_tok = true;
            }
            '(' | ')' => {}
            c if c.is_whitespace() => {
                if in_tok {
                    out.push((std::mem::take(&mut cur), false));
                    in_tok = false;
                }
            }
            '\\' => {
                cur.push('\\');
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_tok = true;
            }
            _ => {
                cur.push(c);
                in_tok = true;
            }
        }
    }
    if quoted {
        return Err("unterminated quoted string".into());
    }
    if in_tok {
        out.push((cur, false));
    }
    Ok(out)
}

/// A <character-string>: escapes resolved, at most 255 octets.
fn char_string(s: &str) -> Result<Vec<u8>, String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            if i + 3 < b.len() && b[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
                let v: u32 = s[i + 1..i + 4].parse().unwrap();
                if v > 255 {
                    return Err("bad \\DDD escape".into());
                }
                out.push(v as u8);
                i += 4;
            } else {
                out.push(b[i + 1]);
                i += 2;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    if out.len() > 255 {
        return Err("character-string longer than 255 octets".into());
    }
    Ok(out)
}

fn push_cs(out: &mut Vec<u8>, s: &[u8]) {
    out.push(s.len() as u8);
    out.extend_from_slice(s);
}

pub fn abs_name(s: &str, origin: &Name) -> Result<Name, String> {
    if s == "@" {
        return Ok(origin.clone());
    }
    if s.ends_with('.') && !s.ends_with("\\.") {
        return Name::parse(s);
    }
    let rel = Name::parse(s)?;
    let mut wire = rel.wire()[..rel.wire().len() - 1].to_vec();
    wire.extend_from_slice(origin.wire());
    if wire.len() > crate::name::MAX_WIRE {
        return Err(format!("{s} is too long under {origin}"));
    }
    Ok(Name::from_wire_unchecked(wire))
}

fn num<T: std::str::FromStr>(s: Option<&(String, bool)>, what: &str) -> Result<T, String> {
    let s = s.ok_or(format!("missing {what}"))?;
    s.0.parse().map_err(|_| format!("bad {what} {:?}", s.0))
}

fn hex(s: &str) -> Result<Vec<u8>, String> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
        return Err("odd-length hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "bad hex".to_string()))
        .collect()
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Seconds, or BIND-style `1h30m`, `2d`, `1w`.
pub fn ttl_value(s: &str) -> Option<u32> {
    if let Ok(v) = s.parse::<u32>() {
        return Some(v);
    }
    let mut total: u64 = 0;
    let mut cur: u64 = 0;
    let mut any = false;
    for c in s.chars() {
        if let Some(d) = c.to_digit(10) {
            cur = cur * 10 + d as u64;
            any = true;
        } else {
            let mul = match c.to_ascii_lowercase() {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86400,
                'w' => 604800,
                _ => return None,
            };
            if !any {
                return None;
            }
            total += cur * mul;
            cur = 0;
            any = false;
        }
    }
    if any {
        return None;
    }
    u32::try_from(total).ok()
}

/// Rdata from its presentation tokens.
pub fn parse_rdata(rtype: u16, toks: &[(String, bool)], origin: &Name) -> Result<Vec<u8>, String> {
    let t = |i: usize| toks.get(i).map(|x| x.0.as_str()).ok_or_else(|| format!("{}: missing field {}", ty::name(rtype), i + 1));
    // RFC 3597 generic form works for every type
    if toks.first().is_some_and(|x| x.0 == "\\#") {
        let len: usize = num(toks.get(1), "\\# length")?;
        let data = hex(&toks[2..].iter().map(|x| x.0.as_str()).collect::<String>())?;
        if data.len() != len {
            return Err("\\# length does not match the data".into());
        }
        return Ok(data);
    }
    let mut out = Vec::new();
    match rtype {
        ty::A => out.extend_from_slice(&t(0)?.parse::<Ipv4Addr>().map_err(|_| "bad IPv4 address")?.octets()),
        ty::AAAA => out.extend_from_slice(&t(0)?.parse::<Ipv6Addr>().map_err(|_| "bad IPv6 address")?.octets()),
        ty::NS | ty::CNAME | ty::DNAME | ty::PTR => out.extend_from_slice(abs_name(t(0)?, origin)?.wire()),
        ty::MX => {
            out.extend_from_slice(&num::<u16>(toks.first(), "preference")?.to_be_bytes());
            out.extend_from_slice(abs_name(t(1)?, origin)?.wire());
        }
        ty::SRV => {
            for (i, w) in ["priority", "weight", "port"].iter().enumerate() {
                out.extend_from_slice(&num::<u16>(toks.get(i), w)?.to_be_bytes());
            }
            out.extend_from_slice(abs_name(t(3)?, origin)?.wire());
        }
        ty::SOA => {
            out.extend_from_slice(abs_name(t(0)?, origin)?.wire());
            out.extend_from_slice(abs_name(t(1)?, origin)?.wire());
            for i in 2..7 {
                let v = ttl_value(t(i)?).ok_or("bad SOA number")?;
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        ty::TXT => {
            if toks.is_empty() {
                return Err("TXT needs at least one string".into());
            }
            for (s, _) in toks {
                // a long unquoted-or-quoted string splits into 255-octet chunks
                let raw = char_string_unbounded(s)?;
                if raw.is_empty() {
                    push_cs(&mut out, &[]);
                }
                for ch in raw.chunks(255) {
                    push_cs(&mut out, ch);
                }
            }
        }
        ty::HINFO => {
            push_cs(&mut out, &char_string(t(0)?)?);
            push_cs(&mut out, &char_string(t(1)?)?);
        }
        ty::CAA => {
            out.push(num::<u8>(toks.first(), "CAA flags")?);
            let tag = t(1)?;
            if tag.is_empty() || tag.len() > 15 || !tag.bytes().all(|c| c.is_ascii_alphanumeric()) {
                return Err("bad CAA tag".into());
            }
            push_cs(&mut out, tag.as_bytes());
            out.extend_from_slice(&char_string_unbounded(t(2)?)?);
        }
        ty::DS | ty::CDS => {
            out.extend_from_slice(&num::<u16>(toks.first(), "key tag")?.to_be_bytes());
            out.push(num::<u8>(toks.get(1), "algorithm")?);
            out.push(num::<u8>(toks.get(2), "digest type")?);
            out.extend_from_slice(&hex(&toks[3..].iter().map(|x| x.0.as_str()).collect::<String>())?);
        }
        ty::DNSKEY | ty::CDNSKEY => {
            out.extend_from_slice(&num::<u16>(toks.first(), "flags")?.to_be_bytes());
            out.push(num::<u8>(toks.get(1), "protocol")?);
            out.push(num::<u8>(toks.get(2), "algorithm")?);
            let k: String = toks[3..].iter().map(|x| x.0.as_str()).collect();
            out.extend_from_slice(&B64.decode(k).map_err(|_| "bad base64 key")?);
        }
        ty::TLSA => {
            for (i, w) in ["usage", "selector", "matching type"].iter().enumerate() {
                out.push(num::<u8>(toks.get(i), w)?);
            }
            out.extend_from_slice(&hex(&toks[3..].iter().map(|x| x.0.as_str()).collect::<String>())?);
        }
        ty::SSHFP => {
            out.push(num::<u8>(toks.first(), "algorithm")?);
            out.push(num::<u8>(toks.get(1), "fingerprint type")?);
            out.extend_from_slice(&hex(&toks[2..].iter().map(|x| x.0.as_str()).collect::<String>())?);
        }
        ty::OPENPGPKEY => {
            let k: String = toks.iter().map(|x| x.0.as_str()).collect();
            out.extend_from_slice(&B64.decode(k).map_err(|_| "bad base64 key")?);
        }
        ty::URI => {
            out.extend_from_slice(&num::<u16>(toks.first(), "priority")?.to_be_bytes());
            out.extend_from_slice(&num::<u16>(toks.get(1), "weight")?.to_be_bytes());
            out.extend_from_slice(&char_string_unbounded(t(2)?)?);
        }
        ty::SVCB | ty::HTTPS => return parse_svcb(toks, origin),
        _ => return Err(format!("{} records need the RFC 3597 form: \\# <len> <hex>", ty::name(rtype))),
    }
    Ok(out)
}

fn char_string_unbounded(s: &str) -> Result<Vec<u8>, String> {
    // reuse the escape logic without the 255 cap
    let mut out = Vec::new();
    for chunk in split_escaped(s) {
        out.extend_from_slice(&char_string(&chunk)?);
    }
    Ok(out)
}

/// Split a long escaped string into pieces short enough for char_string,
/// never inside an escape.
fn split_escaped(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut n = 0;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        cur.push(c);
        if c == '\\' {
            if let Some(d) = it.next() {
                cur.push(d);
                if d.is_ascii_digit() {
                    for _ in 0..2 {
                        if let Some(e) = it.next() {
                            cur.push(e);
                        }
                    }
                }
            }
        }
        n += 1;
        if n >= 200 {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

const SVC_KEYS: &[(u16, &str)] =
    &[(0, "mandatory"), (1, "alpn"), (2, "no-default-alpn"), (3, "port"), (4, "ipv4hint"), (5, "ech"), (6, "ipv6hint")];

fn svc_key(s: &str) -> Option<u16> {
    if let Some(n) = s.strip_prefix("key") {
        return n.parse().ok();
    }
    SVC_KEYS.iter().find(|(_, n)| *n == s).map(|(k, _)| *k)
}

/// SVCB/HTTPS (RFC 9460): priority, target, key=value params.
fn parse_svcb(toks: &[(String, bool)], origin: &Name) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.extend_from_slice(&num::<u16>(toks.first(), "SvcPriority")?.to_be_bytes());
    let target = toks.get(1).ok_or("missing TargetName")?;
    out.extend_from_slice(if target.0 == "." { Name::root() } else { abs_name(&target.0, origin)? }.wire());
    let mut params: Vec<(u16, Vec<u8>)> = Vec::new();
    for (tok, _) in &toks[2..] {
        let (k, v) = match tok.split_once('=') {
            Some((k, v)) => (k, v.trim_matches('"')),
            None => (tok.as_str(), ""),
        };
        let key = svc_key(k).ok_or(format!("unknown SvcParamKey {k}"))?;
        let val = match key {
            0 => v.split(',').map(|x| svc_key(x).ok_or(format!("bad mandatory key {x}"))).collect::<Result<Vec<_>, _>>()?
                .iter()
                .flat_map(|k| k.to_be_bytes())
                .collect(),
            1 => {
                let mut b = Vec::new();
                for a in v.split(',') {
                    if a.is_empty() || a.len() > 255 {
                        return Err("bad alpn".into());
                    }
                    push_cs(&mut b, a.as_bytes());
                }
                b
            }
            2 => Vec::new(),
            3 => v.parse::<u16>().map_err(|_| "bad port")?.to_be_bytes().to_vec(),
            4 => v
                .split(',')
                .map(|a| a.parse::<Ipv4Addr>().map(|x| x.octets().to_vec()).map_err(|_| "bad ipv4hint".to_string()))
                .collect::<Result<Vec<_>, _>>()?
                .concat(),
            5 => B64.decode(v).map_err(|_| "bad ech")?,
            6 => v
                .split(',')
                .map(|a| a.parse::<Ipv6Addr>().map(|x| x.octets().to_vec()).map_err(|_| "bad ipv6hint".to_string()))
                .collect::<Result<Vec<_>, _>>()?
                .concat(),
            _ => char_string_unbounded(v)?,
        };
        params.push((key, val));
    }
    params.sort_by_key(|p| p.0);
    for (k, v) in params {
        out.extend_from_slice(&k.to_be_bytes());
        out.extend_from_slice(&(v.len() as u16).to_be_bytes());
        out.extend_from_slice(&v);
    }
    Ok(out)
}

fn cs_text(b: &[u8]) -> String {
    let mut s = String::from("\"");
    for &c in b {
        match c {
            b'"' | b'\\' => {
                s.push('\\');
                s.push(c as char);
            }
            0x20..=0x7E => s.push(c as char),
            _ => s.push_str(&format!("\\{c:03}")),
        }
    }
    s.push('"');
    s
}

fn read_name_at(d: &[u8], p: &mut usize) -> Option<Name> {
    Name::read(d, p).ok()
}

pub fn type_bitmap_text(b: &[u8]) -> String {
    crate::dnssec::bitmap_types(b).iter().map(|t| ty::name(*t)).collect::<Vec<_>>().join(" ")
}

/// Presentation form of rdata, `\# len hex` for anything unknown or bad.
pub fn to_text(rtype: u16, d: &[u8]) -> String {
    let generic = || format!("\\# {} {}", d.len(), to_hex(d));
    let r = (|| -> Option<String> {
        let mut p = 0;
        Some(match rtype {
            ty::A if d.len() == 4 => Ipv4Addr::new(d[0], d[1], d[2], d[3]).to_string(),
            ty::AAAA if d.len() == 16 => Ipv6Addr::from(<[u8; 16]>::try_from(d).ok()?).to_string(),
            ty::NS | ty::CNAME | ty::DNAME | ty::PTR => read_name_at(d, &mut p)?.to_string(),
            ty::MX => {
                p = 2;
                format!("{} {}", u16::from_be_bytes([d[0], d[1]]), read_name_at(d, &mut p)?)
            }
            ty::SRV => {
                p = 6;
                let n = read_name_at(d, &mut p)?;
                format!(
                    "{} {} {} {}",
                    u16::from_be_bytes([d[0], d[1]]),
                    u16::from_be_bytes([d[2], d[3]]),
                    u16::from_be_bytes([d[4], d[5]]),
                    n
                )
            }
            ty::SOA => {
                let m = read_name_at(d, &mut p)?;
                let r = read_name_at(d, &mut p)?;
                let n: Vec<String> = (0..5)
                    .map(|i| d.get(p + i * 4..p + i * 4 + 4).map(|x| u32::from_be_bytes(x.try_into().unwrap()).to_string()))
                    .collect::<Option<_>>()?;
                format!("{m} {r} {}", n.join(" "))
            }
            ty::TXT | ty::HINFO => {
                let mut parts = Vec::new();
                while p < d.len() {
                    let l = d[p] as usize;
                    parts.push(cs_text(d.get(p + 1..p + 1 + l)?));
                    p += 1 + l;
                }
                parts.join(" ")
            }
            ty::CAA => {
                let tl = *d.get(1)? as usize;
                format!(
                    "{} {} {}",
                    d[0],
                    String::from_utf8_lossy(d.get(2..2 + tl)?),
                    cs_text(d.get(2 + tl..)?)
                )
            }
            ty::DS | ty::CDS if d.len() > 4 => {
                format!("{} {} {} {}", u16::from_be_bytes([d[0], d[1]]), d[2], d[3], to_hex(&d[4..]).to_uppercase())
            }
            ty::DNSKEY | ty::CDNSKEY if d.len() > 4 => {
                format!("{} {} {} {}", u16::from_be_bytes([d[0], d[1]]), d[2], d[3], B64.encode(&d[4..]))
            }
            ty::RRSIG if d.len() > 18 => {
                p = 18;
                let signer = read_name_at(d, &mut p)?;
                format!(
                    "{} {} {} {} {} {} {} {} {}",
                    ty::name(u16::from_be_bytes([d[0], d[1]])),
                    d[2],
                    d[3],
                    u32::from_be_bytes(d[4..8].try_into().ok()?),
                    crate::dnssec::sig_time_text(u32::from_be_bytes(d[8..12].try_into().ok()?)),
                    crate::dnssec::sig_time_text(u32::from_be_bytes(d[12..16].try_into().ok()?)),
                    u16::from_be_bytes([d[16], d[17]]),
                    signer,
                    B64.encode(d.get(p..)?)
                )
            }
            ty::NSEC => {
                let next = read_name_at(d, &mut p)?;
                format!("{next} {}", type_bitmap_text(d.get(p..)?))
            }
            ty::NSEC3 => {
                let n = crate::dnssec::Nsec3::parse_rdata(d)?;
                format!(
                    "{} {} {} {} {} {}",
                    n.hash_alg,
                    n.flags,
                    n.iterations,
                    if n.salt.is_empty() { "-".into() } else { to_hex(&n.salt).to_uppercase() },
                    crate::dnssec::base32hex_encode(&n.next_hash),
                    type_bitmap_text(&n.bitmap)
                )
            }
            ty::NSEC3PARAM if d.len() >= 5 => {
                let sl = d[4] as usize;
                let salt = d.get(5..5 + sl)?;
                format!(
                    "{} {} {} {}",
                    d[0],
                    d[1],
                    u16::from_be_bytes([d[2], d[3]]),
                    if salt.is_empty() { "-".into() } else { to_hex(salt).to_uppercase() }
                )
            }
            ty::TLSA if d.len() > 3 => format!("{} {} {} {}", d[0], d[1], d[2], to_hex(&d[3..])),
            ty::SSHFP if d.len() > 2 => format!("{} {} {}", d[0], d[1], to_hex(&d[2..])),
            ty::OPENPGPKEY => B64.encode(d),
            ty::URI if d.len() > 4 => format!(
                "{} {} {}",
                u16::from_be_bytes([d[0], d[1]]),
                u16::from_be_bytes([d[2], d[3]]),
                cs_text(&d[4..])
            ),
            ty::SVCB | ty::HTTPS if d.len() >= 3 => {
                p = 2;
                let target = read_name_at(d, &mut p)?;
                let mut s = format!("{} {}", u16::from_be_bytes([d[0], d[1]]), target);
                while p + 4 <= d.len() {
                    let k = u16::from_be_bytes([d[p], d[p + 1]]);
                    let l = u16::from_be_bytes([d[p + 2], d[p + 3]]) as usize;
                    let v = d.get(p + 4..p + 4 + l)?;
                    p += 4 + l;
                    let name = SVC_KEYS.iter().find(|(x, _)| *x == k).map(|(_, n)| n.to_string()).unwrap_or(format!("key{k}"));
                    let val = match k {
                        1 => {
                            let mut a = Vec::new();
                            let mut q = 0;
                            while q < v.len() {
                                let l = v[q] as usize;
                                a.push(String::from_utf8_lossy(v.get(q + 1..q + 1 + l)?).to_string());
                                q += 1 + l;
                            }
                            a.join(",")
                        }
                        2 => String::new(),
                        3 if l == 2 => u16::from_be_bytes([v[0], v[1]]).to_string(),
                        4 => v.chunks(4).map(|c| c.try_into().map(|x: [u8; 4]| Ipv4Addr::from(x).to_string()).unwrap_or_default()).collect::<Vec<_>>().join(","),
                        6 => v.chunks(16).map(|c| c.try_into().map(|x: [u8; 16]| Ipv6Addr::from(x).to_string()).unwrap_or_default()).collect::<Vec<_>>().join(","),
                        5 => B64.encode(v),
                        _ => to_hex(v),
                    };
                    s.push(' ');
                    s.push_str(&name);
                    if k != 2 {
                        s.push('=');
                        s.push_str(&val);
                    }
                }
                s
            }
            _ => return None,
        })
    })();
    r.unwrap_or_else(generic)
}

/// Parse one zone-file line: `owner [ttl] [class] type rdata...`.
pub fn parse_line(line: &str, origin: &Name, default_ttl: u32) -> Result<Option<crate::wire::Record>, String> {
    let toks = tokenize(line)?;
    if toks.is_empty() {
        return Ok(None);
    }
    let owner = abs_name(&toks[0].0, origin)?;
    if !owner.is_subdomain_of(origin) {
        return Err(format!("{owner} is outside the zone {origin}"));
    }
    let mut i = 1;
    let mut ttl = default_ttl;
    let mut rtype = None;
    while i < toks.len() && rtype.is_none() {
        let s = &toks[i].0;
        if let Some(v) = ttl_value(s) {
            ttl = v;
        } else if s.eq_ignore_ascii_case("IN") {
        } else if let Some(t) = ty::parse(s) {
            rtype = Some(t);
        } else {
            return Err(format!("unknown type or field {s:?}"));
        }
        i += 1;
    }
    let rtype = rtype.ok_or("missing record type")?;
    if ty::is_meta(rtype) || matches!(rtype, ty::RRSIG | ty::NSEC | ty::NSEC3 | ty::NSEC3PARAM | ty::DNSKEY) {
        return Err(format!("{} records are generated by the server, not written", ty::name(rtype)));
    }
    let data = parse_rdata(rtype, &toks[i..], origin)?;
    Ok(Some(crate::wire::Record::new(owner, rtype, ttl, data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o() -> Name {
        Name::parse("example.com").unwrap()
    }

    fn rt(line: &str) -> String {
        let r = parse_line(line, &o(), 300).unwrap().unwrap();
        format!("{} {} {} {}", r.name, r.ttl, ty::name(r.rtype), to_text(r.rtype, &r.data))
    }

    #[test]
    fn lines_roundtrip() {
        assert_eq!(rt("www A 192.0.2.1"), "www.example.com. 300 A 192.0.2.1");
        assert_eq!(rt("@ 1h IN AAAA 2001:db8::1"), "example.com. 3600 AAAA 2001:db8::1");
        assert_eq!(rt("@ MX 10 mail"), "example.com. 300 MX 10 mail.example.com.");
        assert_eq!(rt("x CNAME other.net."), "x.example.com. 300 CNAME other.net.");
        assert_eq!(rt(r#"@ TXT "v=spf1 -all" "two\"q""#), r#"example.com. 300 TXT "v=spf1 -all" "two\"q""#);
        assert_eq!(rt("_sip._tcp SRV 0 5 5060 sip"), "_sip._tcp.example.com. 300 SRV 0 5 5060 sip.example.com.");
        assert_eq!(
            rt("@ SOA ns1 hostmaster 1 7200 3600 1209600 300"),
            "example.com. 300 SOA ns1.example.com. hostmaster.example.com. 1 7200 3600 1209600 300"
        );
        assert_eq!(rt(r#"@ CAA 0 issue "letsencrypt.org""#), r#"example.com. 300 CAA 0 issue "letsencrypt.org""#);
        assert_eq!(
            rt("@ HTTPS 1 . alpn=h2,h3 ipv4hint=192.0.2.1"),
            "example.com. 300 HTTPS 1 . alpn=h2,h3 ipv4hint=192.0.2.1"
        );
        assert_eq!(rt("x TYPE999 \\# 2 abcd"), "x.example.com. 300 TYPE999 \\# 2 abcd");
        assert!(parse_line("x RRSIG A 13 2 300 0 0 1 x. AAAA", &o(), 300).is_err());
        assert!(parse_line("www.other.org. A 1.2.3.4", &o(), 300).is_err());
        assert!(parse_line("; just a comment", &o(), 300).unwrap().is_none());
    }

    #[test]
    fn long_txt_splits() {
        let long = "a".repeat(600);
        let r = parse_line(&format!("@ TXT \"{long}\""), &o(), 60).unwrap().unwrap();
        assert_eq!(r.data.len(), 600 + 3);
    }
}
