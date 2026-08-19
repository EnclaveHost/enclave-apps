//! The IPNI HTTP-provider engine: turn the site DAG served by the
//! s3-ipfs-adapter into a discoverable HTTP retrieval provider on
//! cid.contact, so third-party gateways (ipfs.io, dweb.link/Rainbow,
//! eth.limo) fetch the content over HTTPS instead of from nan's Kubo over
//! bitswap. See IPNI.md for the Step-0 findings and wire formats.
//!
//! Flow, on a new site root:
//!   1. enumerate the DAG's block multihashes from the adapter's
//!      `?format=car&dag-scope=all` (the content lives in the bucket, not here);
//!   2. build an EntryChunk of those multihashes and an Advertisement (chained
//!      to the previous head) whose Addresses = the adapter gateway and whose
//!      Metadata = transport-ipfs-gateway-http;
//!   3. keep the ad + entry blocks so the indexer can crawl them at
//!      `/ipni/v1/ad/head` and `/ipni/v1/ad/<cid>`;
//!   4. announce the new head to the indexers via `PUT /announce`.
//!
//! Single-threaded, bounded per tick (the suite discipline): the CAR fetch
//! and each announce PUT are one blocking HTTP request per drive() step.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};

use ed25519_dalek::SigningKey;

use crate::ipni::{self, Advertisement};
use crate::multiformats::{hex, hex_decode, multiaddr_encode, varint_read};
use crate::webreq;

const CHAIN_FILE: &str = "/data/ipni-chain.json";
/// Keep at most this many advertisement generations crawlable; older ads are
/// pruned (an indexer that far behind re-syncs from the current head).
const MAX_CHAIN: usize = 32;

// ---- CAR parsing -----------------------------------------------------------

/// Parse a CARv1 stream and return (root CID bytes, block multihashes in DAG
/// order, deduplicated). IPNI entries are multihashes, so that is what we
/// pull from each block's CID. Identity-multihash blocks (0x00) are skipped —
/// a gateway never fetches them (the bytes are inline in the parent).
pub fn car_multihashes(car: &[u8]) -> Result<(Vec<u8>, Vec<Vec<u8>>), String> {
    let mut p = 0usize;
    let (hdr_len, n) = varint_read(&car[p..]).ok_or("car: bad header length")?;
    p += n;
    let hdr_end = p + hdr_len as usize;
    if hdr_end > car.len() {
        return Err("car: truncated header".into());
    }
    let root = car_header_root(&car[p..hdr_end]).ok_or("car: no root in header")?;
    p = hdr_end;

    let mut mhs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while p < car.len() {
        let (sec_len, n) = varint_read(&car[p..]).ok_or("car: bad section length")?;
        p += n;
        let sec_end = p + sec_len as usize;
        if sec_end > car.len() {
            return Err("car: truncated section".into());
        }
        // the section is CID || data; parse the CID to find its multihash
        let (mh, _cid_len) = cid_multihash(&car[p..sec_end]).ok_or("car: bad block CID")?;
        // skip identity multihashes (code 0x00)
        let is_identity = varint_read(&mh).map(|(code, _)| code == 0).unwrap_or(false);
        if !is_identity && seen.insert(mh.clone()) {
            mhs.push(mh);
        }
        p = sec_end;
    }
    Ok((root, mhs))
}

/// The first root CID from a CARv1 dag-cbor header `{roots:[cid],version:1}`.
fn car_header_root(hdr: &[u8]) -> Option<Vec<u8>> {
    // find the tag-42 link (0xd8 0x2a) and read the byte string after it
    let pos = hdr.windows(2).position(|w| w == [0xd8, 0x2a])?;
    let mut q = pos + 2;
    let (major_len, mstr) = cbor_bytestring(&hdr[q..])?;
    q += mstr;
    let raw = &hdr[q..q + major_len];
    // the byte string is 0x00 || cid_bytes (multibase identity prefix)
    if raw.first() != Some(&0x00) {
        return None;
    }
    Some(raw[1..].to_vec())
}

/// Read a CBOR byte-string header at `b`; returns (length, header_size).
fn cbor_bytestring(b: &[u8]) -> Option<(usize, usize)> {
    let first = *b.first()?;
    if first >> 5 != 2 {
        return None; // not a byte string
    }
    let info = first & 0x1f;
    match info {
        0..=23 => Some((info as usize, 1)),
        24 => Some((*b.get(1)? as usize, 2)),
        25 => Some((u16::from_be_bytes([*b.get(1)?, *b.get(2)?]) as usize, 3)),
        26 => {
            let mut a = [0u8; 4];
            a.copy_from_slice(b.get(1..5)?);
            Some((u32::from_be_bytes(a) as usize, 5))
        }
        _ => None,
    }
}

/// Given bytes starting with a binary CID, return (multihash bytes, cid len).
/// CIDv1: version, codec, multihash(code,len,digest). CIDv0: 0x12 0x20 digest.
fn cid_multihash(b: &[u8]) -> Option<(Vec<u8>, usize)> {
    if b.first() == Some(&0x12) && b.get(1) == Some(&0x20) {
        // CIDv0 (sha256 dag-pb): the whole 34 bytes are the multihash
        if b.len() < 34 {
            return None;
        }
        return Some((b[..34].to_vec(), 34));
    }
    let (version, n) = varint_read(b)?;
    if version != 1 {
        return None;
    }
    let (_codec, m) = varint_read(&b[n..])?;
    let mh_start = n + m;
    // multihash = code + len + digest
    let (_code, a) = varint_read(&b[mh_start..])?;
    let (digest_len, c) = varint_read(&b[mh_start + a..])?;
    let mh_len = a + c + digest_len as usize;
    let total = mh_start + mh_len;
    if total > b.len() {
        return None;
    }
    Some((b[mh_start..total].to_vec(), total))
}

// ---- provider engine -------------------------------------------------------

#[derive(Clone, PartialEq, Debug)]
enum PTask {
    /// Fetch the CAR for this root, enumerate multihashes, build + chain the ad.
    Build(Vec<u8>), // root cid bytes
    /// Announce the current head to one indexer.
    Announce(String),
}

pub struct Provider {
    identity: SigningKey,
    peer_id: String,
    gateway_url: String,          // https://ipfs.enclave.host
    retrieval_addr: String,       // /dns4/ipfs.enclave.host/tcp/443/https
    publisher_addrs: Vec<Vec<u8>>, // binary multiaddrs of THIS app (announce Addrs)
    indexers: Vec<String>,
    announce_all_blocks: bool,

    /// Served ad + entry-chunk blocks, keyed by CID bytes.
    blocks: HashMap<Vec<u8>, Vec<u8>>,
    /// Ad CIDs newest-first (for pruning + head).
    chain: VecDeque<Vec<u8>>,
    head: Option<Vec<u8>>,
    current_root: Option<Vec<u8>>,
    block_count: usize,

    tasks: VecDeque<PTask>,
    durable: bool,
    last_note: String,
    last_error: Option<String>,
    announced: Vec<(String, String)>, // (indexer, outcome)
}

impl Provider {
    pub fn new(
        identity: SigningKey,
        gateway_url: String,
        publisher_url: &str,
        indexers: Vec<String>,
        announce_all_blocks: bool,
    ) -> Result<Provider, String> {
        let peer_id = crate::ipns::Identity::from_seed(identity.to_bytes()).peer_id();
        let retrieval_addr = url_to_multiaddr_str(&gateway_url)?;
        let pub_ma = url_to_multiaddr_str(publisher_url)?;
        let pub_bin = multiaddr_encode(&pub_ma).ok_or("bad publisher multiaddr")?;
        let mut p = Provider {
            identity,
            peer_id,
            gateway_url,
            retrieval_addr,
            publisher_addrs: vec![pub_bin],
            indexers,
            announce_all_blocks,
            blocks: HashMap::new(),
            chain: VecDeque::new(),
            head: None,
            current_root: None,
            block_count: 0,
            tasks: VecDeque::new(),
            durable: false,
            last_note: "idle".into(),
            last_error: None,
            announced: Vec::new(),
        };
        p.load_state();
        Ok(p)
    }

    /// Advertise a new site root (called when the IPNS value changes to an
    /// `/ipfs/<cid>`). No-op if it is already the current root.
    pub fn announce_root(&mut self, root_cid_bytes: Vec<u8>) {
        if self.current_root.as_ref() == Some(&root_cid_bytes) && self.head.is_some() {
            return;
        }
        self.tasks.retain(|t| !matches!(t, PTask::Build(_)));
        self.tasks.push_front(PTask::Build(root_cid_bytes));
    }

    /// One bounded step. Returns whether work happened.
    pub fn drive(&mut self) -> bool {
        let Some(task) = self.tasks.pop_front() else { return false };
        match task {
            PTask::Build(root) => self.do_build(root),
            PTask::Announce(indexer) => self.do_announce(&indexer),
        }
        true
    }

    fn do_build(&mut self, root: Vec<u8>) {
        let root_str = ipni::cid_string(&root);
        // 1. enumerate the DAG's block multihashes from the adapter
        let mhs = if self.announce_all_blocks {
            match self.enumerate(&root_str) {
                Ok(m) => m,
                Err(e) => {
                    self.last_error = Some(format!("enumerate {root_str}: {e}"));
                    eprintln!("[ipns-publisher] IPNI {}", self.last_error.as_ref().unwrap());
                    return;
                }
            }
        } else {
            // root-only: announce just the root's multihash
            match cid_multihash(&root) {
                Some((mh, _)) => vec![mh],
                None => {
                    self.last_error = Some("root-only: bad root cid".into());
                    return;
                }
            }
        };
        eprintln!("[ipns-publisher] IPNI: advertising {root_str} ({} blocks)", mhs.len());

        // 2. build the entry chunk + advertisement (chained to the head)
        let entry_block = ipni::entry_chunk(&mhs);
        let entry_cid = ipni::cid_v1(ipni::CODEC_DAG_CBOR, &entry_block);
        let context_id = root.clone(); // one context per site root
        let addrs = vec![self.retrieval_addr.clone()];
        let prev = self.head.clone();
        let ad = Advertisement {
            previous: prev.as_deref(),
            provider: &self.peer_id,
            addresses: &addrs,
            entries: &entry_cid,
            context_id: &context_id,
            metadata: &ipni::METADATA_HTTP_GATEWAY,
            is_rm: false,
        };
        let (ad_block, ad_cid) = ad.build(&self.identity);

        // 3. store the blocks the indexer will crawl
        self.blocks.insert(entry_cid, entry_block);
        self.blocks.insert(ad_cid.clone(), ad_block);
        self.chain.push_front(ad_cid.clone());
        self.head = Some(ad_cid);
        self.current_root = Some(root);
        self.block_count = mhs.len();
        self.prune();
        self.save_state();

        // 4. queue an announce to every indexer
        self.announced.clear();
        for ix in self.indexers.clone() {
            self.tasks.push_back(PTask::Announce(ix));
        }
        self.last_note = format!("advertised {root_str}, {} blocks, chain {}", mhs.len(), self.chain.len());
        self.last_error = None;
    }

    /// Fetch the whole-DAG CAR from the adapter and enumerate block multihashes.
    fn enumerate(&self, root_str: &str) -> Result<Vec<Vec<u8>>, String> {
        let url = webreq::Url::parse(&format!(
            "{}/ipfs/{root_str}?format=car&dag-scope=all",
            self.gateway_url.trim_end_matches('/')
        ))?;
        let (status, body, _) = webreq::request(
            "GET",
            &url,
            &[("accept", "application/vnd.ipld.car")],
            &[],
        )?;
        if status != 200 {
            return Err(format!("adapter answered {status}"));
        }
        let (car_root, mhs) = car_multihashes(&body)?;
        if ipni::cid_string(&car_root) != root_str {
            return Err(format!(
                "CAR root {} != requested {root_str}",
                ipni::cid_string(&car_root)
            ));
        }
        if mhs.is_empty() {
            return Err("CAR held no blocks".into());
        }
        Ok(mhs)
    }

    fn do_announce(&mut self, indexer: &str) {
        let Some(head) = &self.head else { return };
        let outcome = (|| -> Result<String, String> {
            let base = webreq::Url::parse(indexer)?;
            let path = base.path.trim_end_matches('/').to_string();
            let url = base.with_path(format!("{path}/announce"));
            let body = ipni::announce_message(head, &self.publisher_addrs);
            let (status, resp, _) = webreq::request(
                "PUT",
                &url,
                &[("content-type", "application/cbor")],
                &body,
            )?;
            if (200..300).contains(&status) {
                Ok("ok".into())
            } else {
                Err(format!("announce {status}: {:.100}", String::from_utf8_lossy(&resp)))
            }
        })();
        let outcome = match outcome {
            Ok(s) => {
                eprintln!("[ipns-publisher] IPNI: announced head to {indexer}");
                s
            }
            Err(e) => {
                eprintln!("[ipns-publisher] IPNI: {indexer}: {e}");
                e
            }
        };
        self.announced.retain(|(u, _)| u != indexer);
        self.announced.push((indexer.to_string(), outcome));
    }

    fn prune(&mut self) {
        while self.chain.len() > MAX_CHAIN {
            if let Some(old) = self.chain.pop_back() {
                // drop the ad block; its entry chunk is dropped lazily below
                self.blocks.remove(&old);
            }
        }
        // keep only blocks reachable from the retained chain (ads + their entries)
        let mut keep = std::collections::HashSet::new();
        for ad_cid in &self.chain {
            keep.insert(ad_cid.clone());
            if let Some(ad_block) = self.blocks.get(ad_cid) {
                if let Some(entry_cid) = ad_entries_link(ad_block) {
                    keep.insert(entry_cid);
                }
            }
        }
        self.blocks.retain(|k, _| keep.contains(k));
    }

    // ---- serving -----------------------------------------------------------

    pub fn head_topic() -> &'static str {
        ipni::DEFAULT_TOPIC
    }

    /// The `/ipni/v1/ad/head` body (dag-json signed head), or None if no ad yet.
    pub fn serve_head(&self) -> Option<String> {
        let head = self.head.as_ref()?;
        Some(ipni::signed_head(head, None, &self.identity))
    }

    /// A block (ad or entry chunk) by its CID string, dag-cbor bytes.
    pub fn serve_block(&self, cid_str: &str) -> Option<Vec<u8>> {
        let cid = crate::multiformats::base32_decode(cid_str.strip_prefix('b')?)?;
        self.blocks.get(&cid).cloned()
    }

    pub fn is_enabled(&self) -> bool {
        !self.indexers.is_empty()
    }

    pub fn status_json(&self) -> String {
        let announced: Vec<String> = self
            .announced
            .iter()
            .map(|(u, o)| format!("{{\"indexer\":\"{}\",\"outcome\":\"{}\"}}", crate::httpd::json_escape(u), crate::httpd::json_escape(o)))
            .collect();
        format!(
            "{{\"provider\":\"{}\",\"root\":{},\"blocks\":{},\"chainLen\":{},\"head\":{},\"announced\":[{}],\"durable\":{},\"note\":\"{}\"}}",
            self.peer_id,
            self.current_root.as_ref().map(|r| format!("\"{}\"", ipni::cid_string(r))).unwrap_or_else(|| "null".into()),
            self.block_count,
            self.chain.len(),
            self.head.as_ref().map(|h| format!("\"{}\"", ipni::cid_string(h))).unwrap_or_else(|| "null".into()),
            announced.join(","),
            self.durable,
            crate::httpd::json_escape(&self.last_note),
        )
    }

    // ---- persistence -------------------------------------------------------

    fn save_state(&mut self) {
        let mut blocks = String::from("{");
        for (i, (cid, block)) in self.blocks.iter().enumerate() {
            if i > 0 {
                blocks.push(',');
            }
            blocks.push_str(&format!("\"{}\":\"{}\"", ipni::cid_string(cid), hex(block)));
        }
        blocks.push('}');
        let chain: Vec<String> = self.chain.iter().map(|c| format!("\"{}\"", ipni::cid_string(c))).collect();
        let json = format!(
            "{{\"head\":{},\"root\":{},\"chain\":[{}],\"blocks\":{}}}\n",
            self.head.as_ref().map(|h| format!("\"{}\"", ipni::cid_string(h))).unwrap_or_else(|| "null".into()),
            self.current_root.as_ref().map(|r| format!("\"{}\"", ipni::cid_string(r))).unwrap_or_else(|| "null".into()),
            chain.join(","),
            blocks,
        );
        match std::fs::write(CHAIN_FILE, json) {
            Ok(()) => self.durable = true,
            Err(_) => self.durable = false,
        }
    }

    fn load_state(&mut self) {
        let Ok(raw) = std::fs::read_to_string(CHAIN_FILE) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return };
        let cid_of = |s: &str| crate::multiformats::base32_decode(s.strip_prefix('b').unwrap_or(s));
        if let Some(blocks) = v.get("blocks").and_then(|b| b.as_object()) {
            for (cid_str, block_hex) in blocks {
                if let (Some(cid), Some(block)) = (cid_of(cid_str), block_hex.as_str().and_then(hex_decode)) {
                    self.blocks.insert(cid, block);
                }
            }
        }
        if let Some(chain) = v.get("chain").and_then(|c| c.as_array()) {
            for c in chain {
                if let Some(cid) = c.as_str().and_then(cid_of) {
                    self.chain.push_back(cid);
                }
            }
        }
        self.head = v.get("head").and_then(|h| h.as_str()).and_then(cid_of);
        self.current_root = v.get("root").and_then(|r| r.as_str()).and_then(cid_of);
        if self.head.is_some() {
            self.durable = true;
            eprintln!(
                "[ipns-publisher] IPNI: recovered ad chain (head {}, {} ads)",
                self.head.as_ref().map(|h| ipni::cid_string(h)).unwrap_or_default(),
                self.chain.len()
            );
        }
    }
}

/// The entries link CID from an ad block (to know which entry chunk to keep).
fn ad_entries_link(ad_block: &[u8]) -> Option<Vec<u8>> {
    // scan for the "Entries" key (text7) followed by a tag-42 link
    let needle = b"\x67Entries\xd8\x2a"; // text(7)"Entries" tag(42)
    let pos = ad_block.windows(needle.len()).position(|w| w == needle)?;
    let mut q = pos + needle.len();
    let (len, n) = cbor_bytestring_at(&ad_block[q..])?;
    q += n;
    let raw = &ad_block[q..q + len];
    if raw.first() != Some(&0x00) {
        return None;
    }
    Some(raw[1..].to_vec())
}

fn cbor_bytestring_at(b: &[u8]) -> Option<(usize, usize)> {
    cbor_bytestring(b)
}

/// `https://host[:port]` → `/dns4/host/tcp/<port>/https` (or /ip4/…). Only the
/// HTTPS case is used in practice; http maps to /tcp/<port>/http.
fn url_to_multiaddr_str(url: &str) -> Result<String, String> {
    let u = webreq::Url::parse(url)?;
    let proto = if u.host.parse::<std::net::Ipv4Addr>().is_ok() {
        "ip4"
    } else if u.host.parse::<std::net::Ipv6Addr>().is_ok() {
        "ip6"
    } else {
        "dns4"
    };
    let scheme = if u.https { "https" } else { "http" };
    Ok(format!("/{proto}/{}/tcp/{}/{scheme}", u.host, u.port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipni::{cid_v1, CODEC_DAG_CBOR, CODEC_RAW};

    /// Build a tiny CARv1 by hand (header + two raw blocks) and confirm the
    /// parser recovers the root and both block multihashes.
    #[test]
    fn car_parse_roundtrip() {
        // two raw leaf blocks
        let b1 = b"block-one".to_vec();
        let b2 = b"block-two".to_vec();
        let c1 = cid_v1(CODEC_RAW, &b1);
        let c2 = cid_v1(CODEC_RAW, &b2);
        // a dag-cbor "root" block linking them (content irrelevant to the parser)
        let root_block = b"rootdata".to_vec();
        let root = cid_v1(CODEC_DAG_CBOR, &root_block);

        // header: dag-cbor {roots:[root], version:1}
        let mut hdr = Vec::new();
        hdr.push(0xa2); // map(2)
        hdr.push(0x65);
        hdr.extend_from_slice(b"roots");
        hdr.push(0x81); // array(1)
        hdr.push(0xd8);
        hdr.push(0x2a);
        hdr.push(0x58);
        hdr.push((root.len() + 1) as u8);
        hdr.push(0x00);
        hdr.extend_from_slice(&root);
        hdr.push(0x67);
        hdr.extend_from_slice(b"version");
        hdr.push(0x01);

        let mut car = Vec::new();
        let mut push_section = |car: &mut Vec<u8>, cid: &[u8], data: &[u8]| {
            let mut sec = cid.to_vec();
            sec.extend_from_slice(data);
            let mut len = Vec::new();
            crate::multiformats::varint(&mut len, sec.len() as u64);
            car.extend_from_slice(&len);
            car.extend_from_slice(&sec);
        };
        // header
        {
            let mut len = Vec::new();
            crate::multiformats::varint(&mut len, hdr.len() as u64);
            car.extend_from_slice(&len);
            car.extend_from_slice(&hdr);
        }
        push_section(&mut car, &root, &root_block);
        push_section(&mut car, &c1, &b1);
        push_section(&mut car, &c2, &b2);

        let (got_root, mhs) = car_multihashes(&car).unwrap();
        assert_eq!(got_root, root);
        assert_eq!(mhs.len(), 3);
        // each multihash is the CID's multihash portion (0x12 0x20 + digest)
        assert_eq!(mhs[1], &c1[c1.len() - 34..]);
        assert_eq!(mhs[2], &c2[c2.len() - 34..]);
    }

    #[test]
    fn url_to_multiaddr() {
        assert_eq!(url_to_multiaddr_str("https://ipfs.enclave.host").unwrap(), "/dns4/ipfs.enclave.host/tcp/443/https");
        assert_eq!(url_to_multiaddr_str("https://1.2.3.4:8443").unwrap(), "/ip4/1.2.3.4/tcp/8443/https");
    }

    #[test]
    fn ad_entries_link_extraction() {
        let mhs = vec![vec![0x12u8, 0x20].into_iter().chain(std::iter::repeat(1).take(32)).collect::<Vec<u8>>()];
        let entry = ipni::entry_chunk(&mhs);
        let entry_cid = cid_v1(CODEC_DAG_CBOR, &entry);
        let addrs = vec!["/dns4/h/tcp/443/https".to_string()];
        let ad = Advertisement {
            previous: None, provider: "12D3KooTest", addresses: &addrs,
            entries: &entry_cid, context_id: b"ctx", metadata: &ipni::METADATA_HTTP_GATEWAY, is_rm: false,
        };
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let (block, _) = ad.build(&key);
        assert_eq!(ad_entries_link(&block), Some(entry_cid));
    }
}
