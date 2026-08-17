//! The IPFS merkle side of the adapter: CIDv1, multihash, dag-pb, UnixFS and
//! CARv1, hand-rolled from the specs so that what runs is what you read.
//!
//! The import parameters are pinned to kubo's `ipfs add --cid-version 1`
//! defaults and nothing here is configurable, on purpose: 256 KiB chunks,
//! raw leaves, the balanced DAG layout with 174 links per node, sha2-256,
//! CIDv1 base32. Pinning them means the CID this app computes for an object
//! is byte-identical to the CID anyone else gets running `ipfs add` on the
//! same file, so content can be cross-checked, pinned elsewhere, or fetched
//! from any other node that has it, without ever trusting this gateway.
//! (Verified against kubo 0.42 in scripts/e2e.sh, block-for-block.)

use sha2::{Digest, Sha256};

/// kubo's default chunker: size-262144.
pub const CHUNK: u64 = 262144;
/// kubo's DefaultLinksPerBlock for the balanced layout.
pub const FANOUT: usize = 174;

pub const CODEC_RAW: u64 = 0x55;
pub const CODEC_DAG_PB: u64 = 0x70;

// ---- CID -------------------------------------------------------------------

/// A CIDv1 with a sha2-256 multihash, the only kind this app ever mints.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Cid {
    pub codec: u64,
    pub digest: [u8; 32],
}

impl Cid {
    pub fn raw(digest: [u8; 32]) -> Cid {
        Cid { codec: CODEC_RAW, digest }
    }

    pub fn of(codec: u64, data: &[u8]) -> Cid {
        Cid { codec, digest: Sha256::digest(data).into() }
    }

    /// Binary form: varint(1) varint(codec) multihash(0x12, 32, digest).
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(36);
        out.push(0x01);
        varint(&mut out, self.codec);
        out.push(0x12);
        out.push(0x20);
        out.extend_from_slice(&self.digest);
        out
    }

    /// Multibase base32 string form ("b...").
    pub fn to_string(&self) -> String {
        let mut s = String::with_capacity(60);
        s.push('b');
        base32_into(&self.bytes(), &mut s);
        s
    }

    /// Parse the multibase forms this gateway mints (base32 CIDv1, sha2-256).
    pub fn parse(s: &str) -> Option<Cid> {
        let rest = s.strip_prefix('b')?;
        let bytes = base32_decode(rest)?;
        let mut p = 0usize;
        let (version, n) = varint_read(&bytes[p..])?;
        p += n;
        if version != 1 {
            return None;
        }
        let (codec, n) = varint_read(&bytes[p..])?;
        p += n;
        if codec != CODEC_RAW && codec != CODEC_DAG_PB {
            return None;
        }
        if bytes.len() != p + 34 || bytes[p] != 0x12 || bytes[p + 1] != 0x20 {
            return None;
        }
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&bytes[p + 2..]);
        Some(Cid { codec, digest })
    }
}

impl std::fmt::Display for Cid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string())
    }
}

// ---- varint / base32 -------------------------------------------------------

pub fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn varint_read(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for (i, &b) in buf.iter().enumerate().take(10) {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// RFC 4648 base32, lowercase, no padding (the multibase 'b' encoding).
fn base32_into(data: &[u8], out: &mut String) {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // Leftover bits are padding and must be zero.
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

// ---- protobuf primitives ---------------------------------------------------

fn pb_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    varint(out, field << 3 | 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn pb_uint(out: &mut Vec<u8>, field: u64, v: u64) {
    varint(out, field << 3);
    varint(out, v);
}

// ---- dag-pb ----------------------------------------------------------------

#[derive(Clone)]
pub struct Link {
    pub cid: Cid,
    pub name: String,
    pub tsize: u64,
}

/// Encode a PBNode. Per the dag-pb spec (and go-merkledag's wire order),
/// Links (field 2) are written before Data (field 1); every link carries
/// Hash, Name (even when empty) and Tsize, which is what kubo emits.
pub fn dagpb_encode(links: &[Link], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in links {
        let mut msg = Vec::new();
        pb_bytes(&mut msg, 1, &l.cid.bytes());
        pb_bytes(&mut msg, 2, l.name.as_bytes());
        pb_uint(&mut msg, 3, l.tsize);
        pb_bytes(&mut out, 2, &msg);
    }
    pb_bytes(&mut out, 1, data);
    out
}

/// Decode a PBNode we (or kubo) encoded: links + the Data payload.
pub fn dagpb_decode(mut buf: &[u8]) -> Option<(Vec<Link>, Vec<u8>)> {
    let mut links = Vec::new();
    let mut data = Vec::new();
    while !buf.is_empty() {
        let (key, n) = varint_read(buf)?;
        buf = &buf[n..];
        let (field, wire) = (key >> 3, key & 7);
        if wire != 2 {
            let (_, n) = varint_read(buf)?;
            buf = &buf[n..];
            continue;
        }
        let (len, n) = varint_read(buf)?;
        buf = &buf[n..];
        let len = len as usize;
        if buf.len() < len {
            return None;
        }
        let (payload, rest) = buf.split_at(len);
        buf = rest;
        match field {
            1 => data = payload.to_vec(),
            2 => links.push(decode_link(payload)?),
            _ => {}
        }
    }
    Some((links, data))
}

fn decode_link(mut buf: &[u8]) -> Option<Link> {
    let mut cid = None;
    let mut name = String::new();
    let mut tsize = 0u64;
    while !buf.is_empty() {
        let (key, n) = varint_read(buf)?;
        buf = &buf[n..];
        match key {
            0x0a => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                let len = len as usize;
                if buf.len() < len {
                    return None;
                }
                cid = cid_from_bytes(&buf[..len]);
                buf = &buf[len..];
            }
            0x12 => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                let len = len as usize;
                if buf.len() < len {
                    return None;
                }
                name = String::from_utf8_lossy(&buf[..len]).into_owned();
                buf = &buf[len..];
            }
            0x18 => {
                let (v, n) = varint_read(buf)?;
                buf = &buf[n..];
                tsize = v;
            }
            _ => return None,
        }
    }
    Some(Link { cid: cid?, name, tsize })
}

fn cid_from_bytes(b: &[u8]) -> Option<Cid> {
    let (version, n) = varint_read(b)?;
    if version != 1 {
        return None;
    }
    let (codec, m) = varint_read(&b[n..])?;
    let p = n + m;
    if b.len() != p + 34 || b[p] != 0x12 || b[p + 1] != 0x20 {
        return None;
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&b[p + 2..]);
    Some(Cid { codec, digest })
}

// ---- UnixFS ----------------------------------------------------------------

/// UnixFS Data message for a multi-block file node:
/// Type=File(2), filesize, then one blocksize per child, in child order.
pub fn unixfs_file(filesize: u64, blocksizes: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    pb_uint(&mut out, 1, 2);
    pb_uint(&mut out, 3, filesize);
    for &b in blocksizes {
        pb_uint(&mut out, 4, b);
    }
    out
}

/// UnixFS Data message for a directory: Type=Directory(1).
pub fn unixfs_dir() -> Vec<u8> {
    vec![0x08, 0x01]
}

/// Scan a UnixFS Data payload for (Type, filesize).
fn unixfs_scan(mut buf: &[u8]) -> Option<(Option<u64>, Option<u64>)> {
    let mut ty = None;
    let mut filesize = None;
    while !buf.is_empty() {
        let (key, n) = varint_read(buf)?;
        buf = &buf[n..];
        match key & 7 {
            0 => {
                let (v, n) = varint_read(buf)?;
                buf = &buf[n..];
                match key >> 3 {
                    1 => ty = Some(v),
                    3 => filesize = Some(v),
                    _ => {}
                }
            }
            2 => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                if buf.len() < len as usize {
                    return None;
                }
                buf = &buf[len as usize..];
            }
            _ => return None,
        }
    }
    Some((ty, filesize))
}

/// Whether a UnixFS Data payload is a directory (Type == 1).
pub fn is_unixfs_dir(data: &[u8]) -> bool {
    matches!(unixfs_scan(data), Some((Some(1), _)))
}

/// The content length of a UnixFS File payload (Type == 2), else None.
pub fn unixfs_file_size(data: &[u8]) -> Option<u64> {
    match unixfs_scan(data)? {
        (Some(2), size) => Some(size.unwrap_or(0)),
        _ => None,
    }
}

/// The sizes of a file's chunks: all CHUNK except a shorter tail.
/// A zero-byte file is a single empty chunk (kubo mints the empty raw block).
pub fn chunk_sizes(size: u64) -> Vec<u64> {
    if size == 0 {
        return vec![0];
    }
    let n = size.div_ceil(CHUNK);
    let mut v = vec![CHUNK; n as usize];
    *v.last_mut().unwrap() = size - (n - 1) * CHUNK;
    v
}

/// One node of a built DAG: its CID and encoded block.
pub struct BuiltNode {
    pub cid: Cid,
    pub block: Vec<u8>,
}

/// Build the balanced UnixFS DAG for a file from its leaf digests, exactly
/// as kubo's balanced builder shapes it: a single chunk IS the file (the raw
/// leaf, no wrapper); otherwise the root sits at the smallest depth whose
/// capacity (FANOUT^depth leaves) covers the file, and every child is a full
/// subtree of depth-1 except a partial tail, which is still wrapped at every
/// level on the way down (never collapsed).
///
/// Returns (root cid, total dag size, the dag-pb nodes minted). Leaf blocks
/// are not materialized; the caller maps leaf digests back to byte ranges.
pub fn build_file_dag(leaves: &[[u8; 32]], size: u64) -> (Cid, u64, Vec<BuiltNode>) {
    let sizes = chunk_sizes(size);
    debug_assert_eq!(sizes.len(), leaves.len());
    if leaves.len() == 1 {
        return (Cid::raw(leaves[0]), sizes[0], Vec::new());
    }
    let mut depth = 1u32;
    let mut cap = FANOUT as u64;
    while cap < leaves.len() as u64 {
        depth += 1;
        cap *= FANOUT as u64;
    }
    let mut nodes = Vec::new();
    let (cid, tsize, _) = build_level(leaves, &sizes, depth, &mut nodes);
    (cid, tsize, nodes)
}

/// Returns (cid, dag size incl. leaves, content size) of the subtree.
fn build_level(
    leaves: &[[u8; 32]],
    sizes: &[u64],
    depth: u32,
    nodes: &mut Vec<BuiltNode>,
) -> (Cid, u64, u64) {
    if depth == 0 {
        return (Cid::raw(leaves[0]), sizes[0], sizes[0]);
    }
    let group = (FANOUT as u64).pow(depth - 1).max(1) as usize;
    let mut links = Vec::new();
    let mut blocksizes = Vec::new();
    let mut dag_total = 0u64;
    let mut content_total = 0u64;
    for (ls, ss) in leaves.chunks(group).zip(sizes.chunks(group)) {
        let (cid, tsize, content) = build_level(ls, ss, depth - 1, nodes);
        dag_total += tsize;
        content_total += content;
        blocksizes.push(content);
        links.push(Link { cid, name: String::new(), tsize });
    }
    let block = dagpb_encode(&links, &unixfs_file(content_total, &blocksizes));
    let cid = Cid::of(CODEC_DAG_PB, &block);
    dag_total += block.len() as u64;
    nodes.push(BuiltNode { cid, block });
    (cid, dag_total, content_total)
}

/// Build a UnixFS directory node from (name, cid, tsize) entries.
/// Links are sorted by name bytes, as kubo emits them.
/// Returns (cid, dag size incl. this block, the encoded block).
pub fn build_dir(mut entries: Vec<Link>) -> (Cid, u64, Vec<u8>) {
    entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let children: u64 = entries.iter().map(|l| l.tsize).sum();
    let block = dagpb_encode(&entries, &unixfs_dir());
    let cid = Cid::of(CODEC_DAG_PB, &block);
    let tsize = children + block.len() as u64;
    (cid, tsize, block)
}

// ---- CARv1 -----------------------------------------------------------------

/// CARv1 header for a single root: varint length + dag-cbor
/// {"roots": [root], "version": 1} with canonically ordered keys.
pub fn car_header(root: &Cid) -> Vec<u8> {
    let mut cbor = Vec::new();
    cbor.push(0xa2); // map(2)
    cbor.push(0x65); // text(5)
    cbor.extend_from_slice(b"roots");
    cbor.push(0x81); // array(1)
    cbor.push(0xd8); // tag(42)
    cbor.push(0x2a);
    let cid = root.bytes();
    cbor.push(0x58); // bytes(len8)
    cbor.push((cid.len() + 1) as u8);
    cbor.push(0x00); // multibase identity prefix for binary CIDs in cbor
    cbor.extend_from_slice(&cid);
    cbor.push(0x67); // text(7)
    cbor.extend_from_slice(b"version");
    cbor.push(0x01);
    let mut out = Vec::new();
    varint(&mut out, cbor.len() as u64);
    out.extend_from_slice(&cbor);
    out
}

/// One CAR block section: varint(len(cid) + len(data)) + cid + data.
pub fn car_block(out: &mut Vec<u8>, cid: &Cid, data: &[u8]) {
    let cid = cid.bytes();
    varint(out, (cid.len() + data.len()) as u64);
    out.extend_from_slice(&cid);
    out.extend_from_slice(data);
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_rfc4648_vectors() {
        for (input, want) in [
            (&b""[..], ""),
            (b"f", "my"),
            (b"fo", "mzxq"),
            (b"foo", "mzxw6"),
            (b"foob", "mzxw6yq"),
            (b"fooba", "mzxw6ytb"),
            (b"foobar", "mzxw6ytboi"),
        ] {
            let mut s = String::new();
            base32_into(input, &mut s);
            assert_eq!(s, want);
            assert_eq!(base32_decode(want).unwrap(), input);
        }
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u64, 1, 127, 128, 300, 262144, u64::MAX] {
            let mut buf = Vec::new();
            varint(&mut buf, v);
            assert_eq!(varint_read(&buf), Some((v, buf.len())));
        }
    }

    // Cross-checked against kubo 0.42: `ipfs add --cid-version 1 -Q`.
    #[test]
    fn cid_known_vectors() {
        let hello = Cid::of(CODEC_RAW, b"hello world");
        assert_eq!(
            hello.to_string(),
            "bafkreifzjut3te2nhyekklss27nh3k72ysco7y32koao5eei66wof36n5e"
        );
        let empty = Cid::of(CODEC_RAW, b"");
        assert_eq!(
            empty.to_string(),
            "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku"
        );
        assert_eq!(Cid::parse(&hello.to_string()), Some(hello));
        assert_eq!(Cid::parse("bafkreifzjut3te2nhyekklss27nh3k72ysco7y32koao5eei66wof36n5f"), None);
        assert_eq!(Cid::parse("Qmfoo"), None);
    }

    #[test]
    fn dagpb_roundtrip() {
        let leaf = Cid::of(CODEC_RAW, b"abc");
        let links = vec![
            Link { cid: leaf, name: "a.txt".into(), tsize: 3 },
            Link { cid: leaf, name: String::new(), tsize: 3 },
        ];
        let block = dagpb_encode(&links, &unixfs_dir());
        let (got_links, data) = dagpb_decode(&block).unwrap();
        assert_eq!(got_links.len(), 2);
        assert_eq!(got_links[0].name, "a.txt");
        assert_eq!(got_links[0].cid, leaf);
        assert_eq!(got_links[1].tsize, 3);
        assert!(is_unixfs_dir(&data));
        assert!(!is_unixfs_dir(&unixfs_file(5, &[5])));
    }

    #[test]
    fn chunking_math() {
        assert_eq!(chunk_sizes(0), vec![0]);
        assert_eq!(chunk_sizes(1), vec![1]);
        assert_eq!(chunk_sizes(CHUNK), vec![CHUNK]);
        assert_eq!(chunk_sizes(CHUNK + 1), vec![CHUNK, 1]);
        assert_eq!(chunk_sizes(3 * CHUNK), vec![CHUNK, CHUNK, CHUNK]);
    }

    #[test]
    fn single_chunk_file_is_the_leaf() {
        let digest: [u8; 32] = sha2::Sha256::digest(b"tiny").into();
        let (cid, tsize, nodes) = build_file_dag(&[digest], 4);
        assert_eq!(cid, Cid::raw(digest));
        assert_eq!(tsize, 4);
        assert!(nodes.is_empty());
    }

    #[test]
    fn balanced_shape() {
        // 175 chunks: root at depth 2, children [full 174-leaf node, wrapped
        // single-leaf node]. The tail is wrapped, never collapsed.
        let leaves: Vec<[u8; 32]> = (0..175u32)
            .map(|i| sha2::Sha256::digest(i.to_le_bytes()).into())
            .collect();
        let size = 174 * CHUNK + 5;
        let (root, _, nodes) = build_file_dag(&leaves, size);
        assert_eq!(nodes.len(), 3); // two level-1 nodes + the root
        let root_block = &nodes.iter().find(|n| n.cid == root).unwrap().block;
        let (links, _) = dagpb_decode(root_block).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].name, "");
        // The wrapped tail holds exactly one leaf.
        let tail = nodes.iter().find(|n| n.cid == links[1].cid).unwrap();
        let (tail_links, _) = dagpb_decode(&tail.block).unwrap();
        assert_eq!(tail_links.len(), 1);
        assert_eq!(tail_links[0].cid, Cid::raw(leaves[174]));
    }

    #[test]
    fn car_header_shape() {
        let root = Cid::of(CODEC_RAW, b"x");
        let h = car_header(&root);
        let (len, n) = varint_read(&h).unwrap();
        assert_eq!(h.len(), n + len as usize);
        assert_eq!(h[n], 0xa2);
    }
}
