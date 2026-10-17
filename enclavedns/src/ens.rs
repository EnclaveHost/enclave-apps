//! `.eth` names resolved natively from the Ethereum Name Service.
//!
//! The registry is read with `eth_call`s that must agree across a quorum
//! of independent RPC providers, pinned to one block a little behind the
//! head so honest providers see the same state. Wildcard resolvers
//! (ENSIP-10) and offchain resolvers (CCIP-Read, EIP-3668) are followed;
//! offchain answers are checked by the resolver contract's own callback,
//! which is itself a quorum `eth_call`.
//!
//! What a `.eth` name answers in DNS:
//!   * any type the name's resolver stores as DNS records (the ENS DNS
//!     resolver profile, `dnsRecord`), exactly as stored;
//!   * TXT: `dnslink=/ipfs/...` from the contenthash and `eth=0x...` from
//!     the address, unless DNS TXT records are stored; `_dnslink.<name>`
//!     TXT is the DNSLink lookup IPFS clients make;
//!   * A/AAAA with no stored records: a CNAME to `<name>.onion` when the
//!     contenthash is an onion service, else to `<name>.<gateway>` when a
//!     gateway (such as eth.limo) is configured.
//! ENS data is not DNSSEC-signed, so these answers never carry AD.

use crate::name::Name;
use crate::net::{self, Dest};
use crate::resolver::{Answer, Resolver};
use crate::tls;
use crate::wire::{ede, rcode, ty, Record};
use sha3::{Digest, Keccak256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

pub const REGISTRY: &str = "00000000000C2E074eC69A0dFb2997BA6C7d2e1e";

#[derive(Clone)]
pub struct EnsConfig {
    pub rpc: Vec<String>,
    pub quorum: usize,
    pub ttl: u32,
    pub gateway: Option<String>,
}

impl Default for EnsConfig {
    fn default() -> Self {
        EnsConfig {
            rpc: vec![
                "https://ethereum-rpc.publicnode.com".into(),
                "https://eth.drpc.org".into(),
                "https://rpc.flashbots.net".into(),
            ],
            quorum: 2,
            ttl: 300,
            gateway: None,
        }
    }
}

pub fn keccak(b: &[u8]) -> [u8; 32] {
    Keccak256::digest(b).into()
}

pub fn selector(sig: &str) -> [u8; 4] {
    keccak(sig.as_bytes())[..4].try_into().unwrap()
}

/// ENS labels from a DNS name: punycode (`xn--`) decoded, ASCII lowercased.
pub fn ens_labels(name: &Name) -> Result<Vec<String>, String> {
    name.labels()
        .iter()
        .map(|l| {
            let s = std::str::from_utf8(l).map_err(|_| "label is not text")?.to_ascii_lowercase();
            match s.strip_prefix("xn--") {
                Some(p) => punycode_decode(p).ok_or_else(|| format!("bad punycode label {s}")),
                None => Ok(s),
            }
        })
        .collect()
}

pub fn namehash(labels: &[String]) -> [u8; 32] {
    let mut node = [0u8; 32];
    for l in labels.iter().rev() {
        let mut b = node.to_vec();
        b.extend_from_slice(&keccak(l.as_bytes()));
        node = keccak(&b);
    }
    node
}

/// DNS wire encoding of the (Unicode) name, as ENSIP-10 `resolve` takes it.
fn dns_encode(labels: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in labels {
        let b = l.as_bytes();
        if b.len() > 255 {
            // ENSIP-10: an over-long label goes as its hash in brackets
            let h = format!("[{}]", hex(&keccak(b)));
            out.push(h.len() as u8);
            out.extend_from_slice(h.as_bytes());
        } else {
            out.push(b.len() as u8);
            out.extend_from_slice(b);
        }
    }
    out.push(0);
    out
}

/// RFC 3492.
pub fn punycode_decode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    let (mut out, rest): (Vec<char>, &str) = match input.rfind('-') {
        Some(i) => (input[..i].chars().collect(), &input[i + 1..]),
        None => (Vec::new(), input),
    };
    if !out.iter().all(|c| c.is_ascii()) {
        return None;
    }
    let (mut n, mut i, mut bias) = (128u32, 0u32, 72u32);
    let mut it = rest.chars().peekable();
    while it.peek().is_some() {
        let old_i = i;
        let mut w = 1u32;
        let mut k = BASE;
        loop {
            let c = it.next()?;
            let digit = match c {
                'a'..='z' => c as u32 - 'a' as u32,
                'A'..='Z' => c as u32 - 'A' as u32,
                '0'..='9' => c as u32 - '0' as u32 + 26,
                _ => return None,
            };
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias { 1 } else if k >= bias + 26 { 26 } else { k - bias };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let len = out.len() as u32 + 1;
        // adapt
        let mut delta = if old_i == 0 { (i - old_i) / 700 } else { (i - old_i) / 2 };
        delta += delta / len;
        let mut kk = 0;
        while delta > ((BASE - 1) * 26) / 2 {
            delta /= BASE - 1;
            kk += BASE;
        }
        bias = kk + (BASE * delta) / (delta + 38);
        n = n.checked_add(i / len)?;
        i %= len;
        out.insert(i as usize, char::from_u32(n)?);
        i += 1;
    }
    Some(out.into_iter().collect())
}

fn hex(b: &[u8]) -> String {
    crate::rdata::to_hex(b)
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn word(n: usize) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&(n as u64).to_be_bytes());
    w
}

fn padded(b: &[u8]) -> Vec<u8> {
    let mut v = word(b.len()).to_vec();
    v.extend_from_slice(b);
    v.resize(32 + b.len().div_ceil(32) * 32, 0);
    v
}

/// ABI-encode a call whose arguments are all static words or dynamic byte
/// strings, in order.
enum Arg<'a> {
    Word([u8; 32]),
    Bytes(&'a [u8]),
}

fn call_data(sel: [u8; 4], args: &[Arg]) -> Vec<u8> {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let head_len = args.len() * 32;
    for a in args {
        match a {
            Arg::Word(w) => head.extend_from_slice(w),
            Arg::Bytes(b) => {
                head.extend_from_slice(&word(head_len + tail.len()));
                tail.extend_from_slice(&padded(b));
            }
        }
    }
    let mut out = sel.to_vec();
    out.extend(head);
    out.extend(tail);
    out
}

fn read_word(b: &[u8], at: usize) -> Option<usize> {
    let w = b.get(at..at + 32)?;
    if w[..24].iter().any(|&x| x != 0) {
        return None;
    }
    Some(u64::from_be_bytes(w[24..].try_into().ok()?) as usize)
}

/// The dynamic `bytes`/`string` at argument `idx` of an ABI tuple.
fn abi_bytes(b: &[u8], idx: usize) -> Option<Vec<u8>> {
    let off = read_word(b, idx * 32)?;
    let len = read_word(b, off)?;
    b.get(off + 32..off + 32 + len).map(|x| x.to_vec())
}

fn abi_address(b: &[u8], idx: usize) -> Option<[u8; 20]> {
    b.get(idx * 32 + 12..idx * 32 + 32)?.try_into().ok()
}

struct Offchain {
    sender: [u8; 20],
    urls: Vec<String>,
    call_data: Vec<u8>,
    callback: [u8; 4],
    extra: Vec<u8>,
}

fn parse_offchain(rev: &[u8]) -> Option<Offchain> {
    if rev.get(..4)? != selector("OffchainLookup(address,string[],bytes,bytes4,bytes)") {
        return None;
    }
    let b = &rev[4..];
    let sender = abi_address(b, 0)?;
    let uo = read_word(b, 32)?;
    let n = read_word(b, uo)?;
    if n > 8 {
        return None;
    }
    let mut urls = Vec::new();
    for i in 0..n {
        let so = read_word(b, uo + 32 + i * 32)?;
        let at = uo + 32 + so;
        let len = read_word(b, at)?;
        urls.push(String::from_utf8(b.get(at + 32..at + 32 + len)?.to_vec()).ok()?);
    }
    Some(Offchain {
        sender,
        urls,
        call_data: abi_bytes(b, 2)?,
        callback: b.get(96..100)?.try_into().ok()?,
        extra: abi_bytes(b, 4)?,
    })
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum CallResult {
    Ok(Vec<u8>),
    Revert(Vec<u8>),
}

/// What an ENS name holds that DNS can use.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub resolver: Option<String>,
    pub found_at: Option<String>,
    pub wildcard: bool,
    pub addr: Option<String>,
    pub contenthash: Option<String>,
    pub dnslink: Option<String>,
    pub onion: Option<String>,
    pub texts: Vec<(String, String)>,
}

pub struct Ens {
    pub cfg: EnsConfig,
    resolver: Rc<Resolver>,
    block: RefCell<Option<(String, u64)>>,
    cache: RefCell<HashMap<(Name, u16), (Answer, u64)>>,
    pub calls: std::cell::Cell<u64>,
}

impl Ens {
    pub fn new(cfg: EnsConfig, resolver: Rc<Resolver>) -> Rc<Ens> {
        Rc::new(Ens { cfg, resolver, block: RefCell::default(), cache: RefCell::default(), calls: Default::default() })
    }

    /// Where an HTTPS host (an RPC, a CCIP gateway) can be dialled.
    pub async fn dests(&self, host: &str, port: u16) -> Vec<Dest> {
        self.resolver.dests_for(host, port, true).await
    }

    async fn post_json(&self, url: &str, body: &str, limit: Duration) -> Result<serde_json::Value, String> {
        let u = tls::parse_url(url)?;
        let dests = self.dests(&u.host, u.port).await;
        if dests.is_empty() {
            return Err(format!("cannot resolve {}", u.host));
        }
        let r = self
            .resolver
            .http
            .request(&u, &dests, "POST", &[("content-type", "application/json"), ("accept", "application/json")], body.as_bytes(), limit)
            .await
            .map_err(|e| format!("{}: {e}", u.host))?;
        if r.status >= 500 {
            return Err(format!("{} answered HTTP {}", u.host, r.status));
        }
        serde_json::from_slice(&r.body).map_err(|e| format!("{}: bad JSON ({e})", u.host))
    }

    async fn rpc(&self, url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        self.post_json(url, &body, Duration::from_secs(6)).await
    }

    /// The block every provider must answer at: the lowest head reported
    /// by a quorum, two blocks back. Refreshed every 12 s (one slot).
    async fn block(&self) -> Result<String, String> {
        if let Some((b, at)) = self.block.borrow().clone() {
            if net::now_ms().saturating_sub(at) < 12_000 {
                return Ok(b);
            }
        }
        let futs: Vec<_> = self.cfg.rpc.iter().map(|u| self.rpc(u, "eth_blockNumber", serde_json::json!([]))).collect();
        let res = net::join_all(futs).await;
        let mut heads: Vec<u64> = res
            .into_iter()
            .filter_map(|r| r.ok())
            .filter_map(|v| v.get("result").and_then(|x| x.as_str()).and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()))
            .collect();
        if heads.len() < self.cfg.quorum {
            return Err(format!("only {} of {} Ethereum RPCs answered", heads.len(), self.cfg.rpc.len()));
        }
        heads.sort();
        // the quorum-th highest head: a lone stale provider cannot hold everyone back
        let pick = heads[heads.len() - self.cfg.quorum].saturating_sub(2);
        let b = format!("0x{pick:x}");
        *self.block.borrow_mut() = Some((b.clone(), net::now_ms()));
        Ok(b)
    }

    async fn eth_call_one(&self, url: &str, to: &str, data: &[u8], block: &str) -> Result<CallResult, String> {
        let v = self
            .rpc(url, "eth_call", serde_json::json!([{"to": format!("0x{to}"), "data": format!("0x{}", hex(data))}, block]))
            .await?;
        if let Some(r) = v.get("result").and_then(|x| x.as_str()) {
            return unhex(r).map(CallResult::Ok).ok_or("bad hex result".into());
        }
        if let Some(e) = v.get("error") {
            // revert data rides in error.data (a hex string, or nested)
            let d = e.get("data").and_then(|d| d.as_str().map(|s| s.to_string()).or_else(|| d.get("data").and_then(|x| x.as_str()).map(|s| s.to_string())));
            if let Some(d) = d.and_then(|s| unhex(&s)) {
                return Ok(CallResult::Revert(d));
            }
            let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if msg.contains("revert") {
                return Ok(CallResult::Revert(Vec::new()));
            }
            return Err(format!("RPC error: {msg}"));
        }
        Err("RPC answered neither result nor error".into())
    }

    /// One eth_call, accepted only when `quorum` providers return the same.
    async fn eth_call(&self, to: &str, data: &[u8]) -> Result<CallResult, String> {
        let block = self.block().await?;
        self.calls.set(self.calls.get() + 1);
        let futs: Vec<_> = self.cfg.rpc.iter().map(|u| self.eth_call_one(u, to, data, &block)).collect();
        let res = net::join_all(futs).await;
        let oks: Vec<CallResult> = res.iter().filter_map(|r| r.as_ref().ok().cloned()).collect();
        for c in &oks {
            if oks.iter().filter(|x| *x == c).count() >= self.cfg.quorum {
                return Ok(c.clone());
            }
        }
        let errs: Vec<String> = res.into_iter().filter_map(|r| r.err()).collect();
        Err(if oks.len() >= self.cfg.quorum {
            "Ethereum RPC providers disagree".into()
        } else {
            format!("too few Ethereum RPCs answered: {}", errs.join("; "))
        })
    }

    /// eth_call that follows CCIP-Read (EIP-3668) offchain lookups.
    async fn call_ccip(&self, to: &str, data: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        let mut data = data;
        let mut to = to.to_string();
        for _ in 0..4 {
            match self.eth_call(&to, &data).await? {
                CallResult::Ok(r) => return Ok(Some(r)),
                CallResult::Revert(rev) => {
                    let Some(ol) = parse_offchain(&rev) else { return Ok(None) };
                    if hex(&ol.sender) != to.to_ascii_lowercase() {
                        return Err("OffchainLookup sender is not the contract called".into());
                    }
                    let resp = self.ccip_fetch(&ol).await?;
                    data = call_data(ol.callback, &[Arg::Bytes(&resp), Arg::Bytes(&ol.extra)]);
                    to = hex(&ol.sender);
                }
            }
        }
        Err("too many offchain lookups".into())
    }

    async fn ccip_fetch(&self, ol: &Offchain) -> Result<Vec<u8>, String> {
        let sender = format!("0x{}", hex(&ol.sender));
        let data = format!("0x{}", hex(&ol.call_data));
        let mut last = "no gateway URL".to_string();
        for url in &ol.urls {
            if !url.starts_with("https://") {
                last = format!("gateway {url} is not https");
                continue;
            }
            let full = url.replace("{sender}", &sender).replace("{data}", &data);
            let Ok(u) = tls::parse_url(&full) else { continue };
            let dests = self.dests(&u.host, u.port).await;
            let r = if url.contains("{data}") {
                self.resolver.http.request(&u, &dests, "GET", &[("accept", "application/json")], &[], Duration::from_secs(8)).await
            } else {
                let body = serde_json::json!({"data": data, "sender": sender}).to_string();
                self.resolver
                    .http
                    .request(&u, &dests, "POST", &[("content-type", "application/json")], body.as_bytes(), Duration::from_secs(8))
                    .await
            };
            match r {
                Ok(r) if r.status == 200 => {
                    let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|e| format!("gateway JSON: {e}"))?;
                    return v.get("data").and_then(|d| d.as_str()).and_then(unhex).ok_or("gateway answered no data".into());
                }
                Ok(r) if (400..500).contains(&r.status) => return Err(format!("gateway {} refused: HTTP {}", u.host, r.status)),
                Ok(r) => last = format!("gateway {} answered HTTP {}", u.host, r.status),
                Err(e) => last = format!("gateway {}: {e}", u.host),
            }
        }
        Err(last)
    }

    /// The resolver for `labels` (walking up for ENSIP-10 wildcards):
    /// (resolver address hex, labels it was found at, supports resolve()).
    async fn find_resolver(&self, labels: &[String]) -> Result<Option<(String, usize, bool)>, String> {
        let sel = selector("resolver(bytes32)");
        for start in 0..labels.len().saturating_sub(1) {
            let node = namehash(&labels[start..]);
            let r = self.eth_call(REGISTRY, &call_data(sel, &[Arg::Word(node)])).await?;
            let CallResult::Ok(r) = r else { continue };
            let Some(addr) = abi_address(&r, 0) else { continue };
            if addr == [0u8; 20] {
                continue;
            }
            let a = hex(&addr);
            let iface = selector("resolve(bytes,bytes)");
            let mut w = [0u8; 32];
            w[..4].copy_from_slice(&iface);
            let ext = match self.eth_call(&a, &call_data(selector("supportsInterface(bytes4)"), &[Arg::Word(w)])).await? {
                CallResult::Ok(r) => r.get(31).is_some_and(|&b| b == 1),
                CallResult::Revert(_) => false,
            };
            // an ancestor's resolver answers for a descendant only via resolve()
            if start > 0 && !ext {
                return Ok(None);
            }
            return Ok(Some((a, start, ext)));
        }
        Ok(None)
    }

    /// Call a resolver profile function for the name: directly, or
    /// wrapped in ENSIP-10 resolve() for extended resolvers.
    async fn profile(&self, res: &(String, usize, bool), labels: &[String], inner: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        let (addr, _, ext) = res;
        if *ext {
            let wrapped = call_data(selector("resolve(bytes,bytes)"), &[Arg::Bytes(&dns_encode(labels)), Arg::Bytes(&inner)]);
            match self.call_ccip(addr, wrapped).await? {
                Some(r) => Ok(abi_bytes(&r, 0)),
                None => Ok(None),
            }
        } else {
            self.call_ccip(addr, inner).await
        }
    }

    async fn info(&self, labels: &[String], want_texts: &[&str]) -> Result<Option<Info>, String> {
        let Some(res) = self.find_resolver(labels).await? else { return Ok(None) };
        let node = namehash(labels);
        let mut info = Info {
            resolver: Some(format!("0x{}", res.0)),
            found_at: Some(labels[res.1..].join(".")),
            wildcard: res.1 > 0,
            ..Default::default()
        };
        // address and contenthash in parallel: each is a quorum round trip
        let mut both = net::join_all(vec![
            self.profile(&res, labels, call_data(selector("addr(bytes32)"), &[Arg::Word(node)])),
            self.profile(&res, labels, call_data(selector("contenthash(bytes32)"), &[Arg::Word(node)])),
        ])
        .await
        .into_iter();
        let (addr, ch) = (both.next().unwrap(), both.next().unwrap());
        if let Some(r) = addr? {
            if let Some(a) = abi_address(&r, 0).filter(|a| *a != [0u8; 20]) {
                info.addr = Some(checksum_address(&a));
            }
        }
        if let Some(r) = ch? {
            if let Some(ch) = abi_bytes(&r, 0).filter(|c| !c.is_empty()) {
                let (text, dnslink, onion) = decode_contenthash(&ch);
                info.contenthash = Some(text);
                info.dnslink = dnslink;
                info.onion = onion;
            }
        }
        for key in want_texts {
            let inner = call_data(selector("text(bytes32,string)"), &[Arg::Word(node), Arg::Bytes(key.as_bytes())]);
            if let Some(r) = self.profile(&res, labels, inner).await? {
                if let Some(v) = abi_bytes(&r, 0).and_then(|b| String::from_utf8(b).ok()).filter(|s| !s.is_empty()) {
                    info.texts.push((key.to_string(), v));
                }
            }
        }
        Ok(Some(info))
    }

    /// DNS records the resolver stores for (qname, qtype), wire format.
    async fn dns_records(&self, res: &(String, usize, bool), labels: &[String], qname: &Name, qtype: u16) -> Result<Vec<Record>, String> {
        let dnsname = keccak(qname.to_lowercase().wire());
        let mut q = [0u8; 32];
        q[30..].copy_from_slice(&qtype.to_be_bytes());
        // for a wildcard or extended resolver, the record lives under the name
        // itself; for a plain resolver found above, under the ENS name it serves
        let zone = if res.2 { labels.to_vec() } else { labels[res.1..].to_vec() };
        let inner = call_data(selector("dnsRecord(bytes32,bytes32,uint16)"), &[Arg::Word(namehash(&zone)), Arg::Word(dnsname), Arg::Word(q)]);
        let Some(r) = self.profile(res, labels, inner).await? else { return Ok(Vec::new()) };
        let Some(wire) = abi_bytes(&r, 0) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        let mut p = 0;
        while p < wire.len() {
            let Ok(owner) = Name::read(&wire, &mut p) else { break };
            let Some(h) = wire.get(p..p + 10) else { break };
            let (t, ttl, len) = (u16::from_be_bytes([h[0], h[1]]), u32::from_be_bytes(h[4..8].try_into().unwrap()), u16::from_be_bytes([h[8], h[9]]) as usize);
            let Some(d) = wire.get(p + 10..p + 10 + len) else { break };
            p += 10 + len;
            if owner == *qname && (t == qtype || t == ty::CNAME) && !ty::is_meta(t) {
                out.push(Record::new(qname.clone(), t, ttl.min(self.cfg.ttl.max(60)), d.to_vec()));
            }
        }
        Ok(out)
    }

    /// Resolve the RPC hosts and pin a block now, so the first `.eth`
    /// query does not pay for a cold cache.
    pub async fn warm(&self) {
        for u in &self.cfg.rpc {
            if let Ok(u) = tls::parse_url(u) {
                self.dests(&u.host, u.port).await;
            }
        }
        if let Err(e) = self.block().await {
            eprintln!("[enclavedns] ENS: {e}");
        }
    }

    /// The JSON-API view of a name.
    pub async fn describe(&self, name: &Name) -> Result<Option<Info>, String> {
        let labels = ens_labels(name)?;
        self.info(&labels, &["url", "avatar", "description", "email", "com.twitter", "com.github"]).await
    }

    /// Answer (qname, qtype) for a name under `eth.`.
    pub async fn answer(&self, qname: &Name, qtype: u16) -> Answer {
        let now = net::now_ms();
        if let Some((a, exp)) = self.cache.borrow().get(&(qname.to_lowercase(), qtype)) {
            if *exp > now {
                let mut a = a.clone();
                a.source = "cache";
                return a;
            }
        }
        let a = match self.answer_uncached(qname, qtype).await {
            Ok(a) => a,
            Err(e) => {
                let mut a = Answer::fail(ede::NETWORK_ERROR, format!("ENS: {e}"));
                a.source = "ens";
                a
            }
        };
        let ttl = if a.rcode == rcode::SERVFAIL { 10 } else { a.ttl as u64 };
        let mut c = self.cache.borrow_mut();
        if c.len() > 10_000 {
            c.clear();
        }
        c.insert((qname.to_lowercase(), qtype), (a.clone(), now + ttl * 1000));
        a
    }

    async fn answer_uncached(&self, qname: &Name, qtype: u16) -> Result<Answer, String> {
        let ttl = self.cfg.ttl;
        let mut a = Answer::negative(rcode::NOERROR, ttl, "ens");
        let mut labels = ens_labels(qname)?;
        if labels.len() < 2 {
            // `eth.` itself: no records we serve
            return Ok(a);
        }
        // _dnslink.<name> is a DNSLink lookup of <name>
        let dnslink_query = labels[0] == "_dnslink";
        if dnslink_query {
            labels.remove(0);
        }
        let Some(res) = self.find_resolver(&labels).await? else {
            a.rcode = rcode::NXDOMAIN;
            a.ede = Some((ede::OTHER, "no ENS resolver for this name".into()));
            return Ok(a);
        };
        if !dnslink_query && qtype != ty::TXT {
            let recs = self.dns_records(&res, &labels, qname, qtype).await.unwrap_or_default();
            if !recs.is_empty() {
                a.answer = recs;
                a.ttl = a.answer.iter().map(|r| r.ttl).min().unwrap_or(ttl);
                return Ok(a);
            }
        }
        let txt = |s: &str| -> Vec<u8> {
            let mut d = Vec::new();
            for ch in s.as_bytes().chunks(255) {
                d.push(ch.len() as u8);
                d.extend_from_slice(ch);
            }
            d
        };
        match qtype {
            ty::TXT => {
                if !dnslink_query {
                    let recs = self.dns_records(&res, &labels, qname, ty::TXT).await.unwrap_or_default();
                    if !recs.is_empty() {
                        a.answer = recs;
                        return Ok(a);
                    }
                }
                let info = self.info(&labels, &[]).await?.unwrap_or_default();
                if let Some(dl) = &info.dnslink {
                    a.answer.push(Record::new(qname.clone(), ty::TXT, ttl, txt(&format!("dnslink={dl}"))));
                }
                if !dnslink_query {
                    if let Some(addr) = &info.addr {
                        a.answer.push(Record::new(qname.clone(), ty::TXT, ttl, txt(&format!("eth={addr}"))));
                    }
                }
            }
            ty::A | ty::AAAA if !dnslink_query => {
                let info = self.info(&labels, &[]).await?.unwrap_or_default();
                let target = match (&info.onion, &self.cfg.gateway) {
                    (Some(o), _) => Name::parse(&format!("{o}.onion")).ok(),
                    (None, Some(g)) if info.contenthash.is_some() => Name::parse(&format!("{}.{g}", labels.join("."))).ok(),
                    _ => None,
                };
                if let Some(t) = target {
                    a.answer.push(Record::new(qname.clone(), ty::CNAME, ttl, t.wire().to_vec()));
                }
            }
            _ => {}
        }
        Ok(a)
    }
}

/// EIP-55 mixed-case checksum.
pub fn checksum_address(a: &[u8; 20]) -> String {
    let h = hex(a);
    let k = keccak(h.as_bytes());
    let mut out = String::from("0x");
    for (i, c) in h.chars().enumerate() {
        let nib = (k[i / 2] >> (if i % 2 == 0 { 4 } else { 0 })) & 0xF;
        out.push(if c.is_ascii_alphabetic() && nib >= 8 { c.to_ascii_uppercase() } else { c });
    }
    out
}

fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &x) in b.iter().enumerate().take(9) {
        v |= ((x & 0x7F) as u64) << (7 * i);
        if x & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// ENSIP-7 contenthash -> (display text, DNSLink path, onion address).
pub fn decode_contenthash(ch: &[u8]) -> (String, Option<String>, Option<String>) {
    let Some((codec, n)) = varint(ch) else { return (format!("0x{}", hex(ch)), None, None) };
    let rest = &ch[n..];
    let cid_v1 = |cid: &[u8]| -> String {
        // a bare sha2-256 multihash is CIDv0: write it as CIDv1 dag-pb
        let mut c = cid.to_vec();
        if c.len() == 34 && c[0] == 0x12 && c[1] == 0x20 {
            let mut v = vec![0x01, 0x70];
            v.extend_from_slice(&c);
            c = v;
        }
        format!("b{}", crate::dnssec::base32_encode(&c, B32))
    };
    match codec {
        0xe3 => {
            let p = format!("/ipfs/{}", cid_v1(rest));
            (format!("ipfs://{}", &p[6..]), Some(p), None)
        }
        0xe5 => {
            let p = format!("/ipns/{}", cid_v1(rest));
            (format!("ipns://{}", &p[6..]), Some(p), None)
        }
        0xe4 => (format!("bzz://{}", hex(rest)), None, None),
        0x01bd => {
            // onion3: the 56-character address as text, or its 35 raw bytes
            let addr = if rest.len() == 56 && rest.is_ascii() {
                String::from_utf8_lossy(rest).to_ascii_lowercase()
            } else if rest.len() == 35 {
                crate::dnssec::base32_encode(rest, B32)
            } else {
                return (format!("0x{}", hex(ch)), None, None);
            };
            (format!("onion3://{addr}"), None, Some(addr))
        }
        0xb29910 => {
            use base64::Engine;
            (format!("ar://{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rest)), None, None)
        }
        _ => (format!("0x{}", hex(ch)), None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namehash_eip137() {
        assert_eq!(hex(&namehash(&[])), "0".repeat(64));
        assert_eq!(hex(&namehash(&["eth".into()])), "93cdeb708b7545dc668eb9280176169d1c33cfd8ed6f04690a0bcc88a93fc4ae");
        assert_eq!(
            hex(&namehash(&["foo".into(), "eth".into()])),
            "de9b09fd7c5f901e23a3f19fecc54828e9c848539801e86591bd9801b019f84f"
        );
    }

    #[test]
    fn selectors() {
        assert_eq!(hex(&selector("resolver(bytes32)")), "0178b8bf");
        assert_eq!(hex(&selector("addr(bytes32)")), "3b3b57de");
        assert_eq!(hex(&selector("contenthash(bytes32)")), "bc1c58d1");
        assert_eq!(hex(&selector("resolve(bytes,bytes)")), "9061b923");
        assert_eq!(hex(&selector("supportsInterface(bytes4)")), "01ffc9a7");
        assert_eq!(hex(&selector("OffchainLookup(address,string[],bytes,bytes4,bytes)")), "556f1830");
    }

    #[test]
    fn punycode() {
        assert_eq!(punycode_decode("mnchen-3ya").unwrap(), "münchen");
        assert_eq!(punycode_decode("ls8h").unwrap(), "💩");
        let n = Name::parse("xn--ls8h.eth").unwrap();
        assert_eq!(ens_labels(&n).unwrap(), vec!["💩".to_string(), "eth".to_string()]);
    }

    #[test]
    fn contenthash_ipfs() {
        // ENSIP-7 example: ipfs://bafybeibj6lixxzqtsb45ysdjnupvqkufgdvzqbnvmhw2kf7cfkesy7r7d4
        let ch = unhex("e3010170122029f2d17be6139079dc48696d1f582a8530eb9805b561eda517e22a892c7e3f1f").unwrap();
        let (text, dl, _) = decode_contenthash(&ch);
        assert_eq!(text, "ipfs://bafybeibj6lixxzqtsb45ysdjnupvqkufgdvzqbnvmhw2kf7cfkesy7r7d4");
        assert_eq!(dl.unwrap(), "/ipfs/bafybeibj6lixxzqtsb45ysdjnupvqkufgdvzqbnvmhw2kf7cfkesy7r7d4");
    }

    #[test]
    fn eip55() {
        let a: [u8; 20] = unhex("5aaeb6053f3e94c9b9a09f33669435e7ef1beaed").unwrap().try_into().unwrap();
        assert_eq!(checksum_address(&a), "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed");
    }

    #[test]
    fn abi_roundtrip() {
        let d = call_data(selector("resolve(bytes,bytes)"), &[Arg::Bytes(b"\x03foo\x03eth\x00"), Arg::Bytes(&[1, 2, 3])]);
        assert_eq!(abi_bytes(&d[4..], 0).unwrap(), b"\x03foo\x03eth\x00");
        assert_eq!(abi_bytes(&d[4..], 1).unwrap(), vec![1, 2, 3]);
    }
}
