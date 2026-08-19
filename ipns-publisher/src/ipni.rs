//! IPNI (InterPlanetary Network Indexer) provider codec: the advertisement
//! chain, entry chunks, the advertisement signature, the signed head, and
//! the announce message — everything needed to announce the s3-ipfs-adapter
//! as a discoverable HTTP retrieval provider to cid.contact.
//!
//! Hand-rolled and verified BYTE-FOR-BYTE against go-libipni v0.8.2 (the
//! reference implementation) in the tests below; the vectors are generated
//! by scripts/ipni-vectors/ from a fixed key. See IPNI.md for the Step-0
//! findings and the exact wire formats this matches.
//!
//! Two addresses, never conflated: an Advertisement's `Addresses` is where a
//! client fetches CONTENT (the adapter's trustless gateway), while the
//! announce message's `Addrs` is where the indexer fetches the AD CHAIN
//! (this app's own HTTP endpoint).

#![allow(dead_code)]

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::ipns::{pb_bytes, pubkey_protobuf};
use crate::multiformats::{base32, base64_nopad, varint};

/// transport-ipfs-gateway-http: uvarint(0x0920), no trailing metadata.
pub const METADATA_HTTP_GATEWAY: [u8; 2] = [0xA0, 0x12];

pub const CODEC_DAG_CBOR: u64 = 0x71;
pub const CODEC_RAW: u64 = 0x55;

const AD_SIG_DOMAIN: &str = "indexer";
const AD_SIG_CODEC: &str = "/indexer/ingest/adSignature";
pub const DEFAULT_TOPIC: &str = "/indexer/ingest/mainnet";

// ---- CID -------------------------------------------------------------------

/// Binary CIDv1 for a block of the given codec: 0x01 codec 0x12 0x20 digest.
pub fn cid_v1(codec: u64, block: &[u8]) -> Vec<u8> {
    let digest = Sha256::digest(block);
    let mut out = Vec::with_capacity(40);
    out.push(0x01);
    varint(&mut out, codec);
    out.push(0x12);
    out.push(0x20);
    out.extend_from_slice(&digest);
    out
}

/// The multibase base32 string form ("bafy…"/"bafk…").
pub fn cid_string(cid_bytes: &[u8]) -> String {
    format!("b{}", base32(cid_bytes))
}

// ---- minimal dag-cbor encoder ----------------------------------------------

fn cbor_head(out: &mut Vec<u8>, major: u8, v: u64) {
    let m = major << 5;
    match v {
        0..=23 => out.push(m | v as u8),
        24..=0xff => {
            out.push(m | 24);
            out.push(v as u8);
        }
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend_from_slice(&(v as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}
fn cbor_uint(out: &mut Vec<u8>, v: u64) {
    cbor_head(out, 0, v);
}
fn cbor_bytes(out: &mut Vec<u8>, b: &[u8]) {
    cbor_head(out, 2, b.len() as u64);
    out.extend_from_slice(b);
}
fn cbor_text(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}
fn cbor_array(out: &mut Vec<u8>, n: u64) {
    cbor_head(out, 4, n);
}
fn cbor_map(out: &mut Vec<u8>, n: u64) {
    cbor_head(out, 5, n);
}
fn cbor_bool(out: &mut Vec<u8>, b: bool) {
    out.push(if b { 0xf5 } else { 0xf4 });
}
/// A dag-cbor CID link: tag(42) + byte-string(0x00 || cid_bytes).
fn cbor_link(out: &mut Vec<u8>, cid_bytes: &[u8]) {
    out.push(0xd8);
    out.push(0x2a); // tag 42
    cbor_head(out, 2, (cid_bytes.len() + 1) as u64);
    out.push(0x00); // multibase identity prefix for binary CIDs in cbor
    out.extend_from_slice(cid_bytes);
}

// ---- EntryChunk ------------------------------------------------------------

/// A single EntryChunk block: `{entries: [<multihash>...]}` (no Next). One
/// chunk holds ~120k multihashes under the 4 MiB ingest cap, enough for any
/// realistic site DAG; chaining (Next) is a future extension.
pub fn entry_chunk(multihashes: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_map(&mut out, 1);
    cbor_text(&mut out, "Entries");
    cbor_array(&mut out, multihashes.len() as u64);
    for mh in multihashes {
        cbor_bytes(&mut out, mh);
    }
    out
}

// ---- Advertisement ---------------------------------------------------------

pub struct Advertisement<'a> {
    pub previous: Option<&'a [u8]>, // CID bytes of the previous ad
    pub provider: &'a str,          // our ed25519 peer id string
    pub addresses: &'a [String],    // retrieval multiaddr strings
    pub entries: &'a [u8],          // CID bytes of the entry chunk
    pub context_id: &'a [u8],
    pub metadata: &'a [u8],
    pub is_rm: bool,
}

impl Advertisement<'_> {
    /// The data signed: multihash(sha256( previd ‖ entries ‖ provider ‖
    /// addresses ‖ metadata ‖ isRm )). Matches go-libipni signaturePayload.
    fn signature_payload(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(prev) = self.previous {
            buf.extend_from_slice(prev);
        }
        buf.extend_from_slice(self.entries);
        buf.extend_from_slice(self.provider.as_bytes());
        for a in self.addresses {
            buf.extend_from_slice(a.as_bytes());
        }
        buf.extend_from_slice(self.metadata);
        buf.push(if self.is_rm { 1 } else { 0 });
        let digest = Sha256::digest(&buf);
        let mut mh = Vec::with_capacity(34);
        mh.push(0x12);
        mh.push(0x20);
        mh.extend_from_slice(&digest);
        mh
    }

    /// The Signature field: a libp2p signed envelope over the payload.
    pub fn sign(&self, key: &SigningKey) -> Vec<u8> {
        let payload = self.signature_payload();
        signed_envelope(key, AD_SIG_DOMAIN, AD_SIG_CODEC.as_bytes(), &payload)
    }

    /// The dag-cbor block, keys in canonical (length, then bytewise) order:
    /// IsRm, entries, Metadata, Provider, Addresses, ContextID, Signature,
    /// [PreviousID]. Matches go-libipni's Advertisement.ToNode() exactly.
    pub fn encode(&self, signature: &[u8]) -> Vec<u8> {
        let n = if self.previous.is_some() { 8 } else { 7 };
        let mut out = Vec::new();
        cbor_map(&mut out, n);
        cbor_text(&mut out, "IsRm");
        cbor_bool(&mut out, self.is_rm);
        cbor_text(&mut out, "Entries");
        cbor_link(&mut out, self.entries);
        cbor_text(&mut out, "Metadata");
        cbor_bytes(&mut out, self.metadata);
        cbor_text(&mut out, "Provider");
        cbor_text(&mut out, self.provider);
        cbor_text(&mut out, "Addresses");
        cbor_array(&mut out, self.addresses.len() as u64);
        for a in self.addresses {
            cbor_text(&mut out, a);
        }
        cbor_text(&mut out, "ContextID");
        cbor_bytes(&mut out, self.context_id);
        cbor_text(&mut out, "Signature");
        cbor_bytes(&mut out, signature);
        if let Some(prev) = self.previous {
            cbor_text(&mut out, "PreviousID");
            cbor_link(&mut out, prev);
        }
        out
    }

    /// Sign and encode in one step; returns (block, cid_bytes).
    pub fn build(&self, key: &SigningKey) -> (Vec<u8>, Vec<u8>) {
        let sig = self.sign(key);
        let block = self.encode(&sig);
        let cid = cid_v1(CODEC_DAG_CBOR, &block);
        (block, cid)
    }
}

// ---- libp2p signed envelope ------------------------------------------------

/// A libp2p record.Envelope: protobuf {1: PublicKey, 2: payloadType,
/// 3: payload, 5: signature}, sig = ed25519(varint-len-prefixed(domain ‖
/// payloadType ‖ payload)).
pub fn signed_envelope(key: &SigningKey, domain: &str, payload_type: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut unsigned = Vec::new();
    for field in [domain.as_bytes(), payload_type, payload] {
        varint(&mut unsigned, field.len() as u64);
        unsigned.extend_from_slice(field);
    }
    let sig = key.sign(&unsigned).to_bytes();
    let pubkey_pb = pubkey_protobuf(key.verifying_key().as_bytes());

    let mut out = Vec::new();
    pb_bytes(&mut out, 1, &pubkey_pb);
    pb_bytes(&mut out, 2, payload_type);
    pb_bytes(&mut out, 3, payload);
    pb_bytes(&mut out, 5, &sig);
    out
}

// ---- SignedHead (dag-json) -------------------------------------------------

/// The `/ipni/v1/ad/head` response: dag-json
/// `{"head":{"/":"<cid>"},"pubkey":{"/":{"bytes":"<b64>"}},"sig":{"/":{"bytes":"<b64>"}}}`,
/// sig = ed25519(head_cid_bytes ‖ topic-utf8). Topic omitted when default.
pub fn signed_head(head_cid: &[u8], topic: Option<&str>, key: &SigningKey) -> String {
    let mut sig_buf = head_cid.to_vec();
    if let Some(t) = topic {
        sig_buf.extend_from_slice(t.as_bytes());
    }
    let sig = key.sign(&sig_buf).to_bytes();
    let pubkey_pb = pubkey_protobuf(key.verifying_key().as_bytes());
    let cid = cid_string(head_cid);
    let mut json = format!("{{\"head\":{{\"/\":\"{cid}\"}}");
    if let Some(t) = topic {
        json.push_str(&format!(",\"topic\":\"{t}\""));
    }
    json.push_str(&format!(
        ",\"pubkey\":{{\"/\":{{\"bytes\":\"{}\"}}}},\"sig\":{{\"/\":{{\"bytes\":\"{}\"}}}}}}",
        base64_nopad(&pubkey_pb),
        base64_nopad(&sig),
    ));
    json
}

// ---- announce message ------------------------------------------------------

/// The `PUT /announce` body: a cbor-gen tuple `[Cid, Addrs, ExtraData]`
/// (3-array). Cid = the head ad CID (tag-42 link); Addrs = binary multiaddrs
/// of THIS app's publisher endpoint; ExtraData empty.
pub fn announce_message(head_cid: &[u8], addrs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_array(&mut out, 3);
    cbor_link(&mut out, head_cid);
    cbor_array(&mut out, addrs.len() as u64);
    for a in addrs {
        cbor_bytes(&mut out, a);
    }
    cbor_bytes(&mut out, &[]); // ExtraData
    out
}

// ---- tests: byte-for-byte vs go-libipni v0.8.2 -----------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiformats::{hex, hex_decode};

    // Vectors from scripts/ipni-vectors/ (go-libipni v0.8.2, seed = 32 x 0x11).
    const SEED: [u8; 32] = [0x11; 32];
    const PROVIDER: &str = "12D3KooWPqT2nMDSiXUSx5D7fasaxhxKigVhcqfkKqrLghCq9jxz";
    const MH1: &str = "122053a62ac318fdb8bc3cd74b0c8fd0d724e04f6c3e3bb4e647a0a327ecdfe30c71";
    const MH2: &str = "1220f73fdc146904b6ab4e0ff10e151b30f3f3b2db3a06b8530b1de681ee3f47eff2";
    const ENTRYCHUNK_DAGCBOR: &str = "a167456e7472696573825822122053a62ac318fdb8bc3cd74b0c8fd0d724e04f6c3e3bb4e647a0a327ecdfe30c7158221220f73fdc146904b6ab4e0ff10e151b30f3f3b2db3a06b8530b1de681ee3f47eff2";
    const ENTRYCHUNK_CID: &str = "bafyreicraefw7sl4jaeym3dqh6aojahuhqrfayes2osll7bcguzbvwimn4";
    const AD_SIGNATURE: &str = "0a2408011220d04ab232742bb4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c9778737121b2f696e64657865722f696e676573742f61645369676e61747572651a221220aecb1808130f9c1ea68204cdd8410c602be367bfb8e9e8f78f8324a42f0c1c512a40f5555690ef30bf685e47663546e808a419fea1c40707870b5d4e5c049feef3a69827065d162a6f22dcbdd311e604cfa3baa6f013789dc3ae6e64a6a54fc8780e";
    const AD_DAGCBOR: &str = "a7644973526df467456e7472696573d82a5825000171122051010b6fc97c4809866c703f80e480f43c22506092d3a4b5fc2235321ad90c6f684d6574616461746142a0126850726f76696465727834313244334b6f6f57507154326e4d44536958555378354437666173617868784b696756686371666b4b71724c67684371396a787a694164647265737365738178252f646e73342f697066732e656e636c6176652e686f73742f7463702f3434332f687474707369436f6e74657874494451736974652d726f6f742d636f6e74657874695369676e617475726558a90a2408011220d04ab232742bb4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c9778737121b2f696e64657865722f696e676573742f61645369676e61747572651a221220aecb1808130f9c1ea68204cdd8410c602be367bfb8e9e8f78f8324a42f0c1c512a40f5555690ef30bf685e47663546e808a419fea1c40707870b5d4e5c049feef3a69827065d162a6f22dcbdd311e604cfa3baa6f013789dc3ae6e64a6a54fc8780e";
    const AD_CID: &str = "bafyreih63i6syusrdbczzx66x3jdggdpctjyvc5q3cujagynyjspgxmt5u";
    const AD2_DAGCBOR: &str = "a8644973526df467456e7472696573d82a5825000171122051010b6fc97c4809866c703f80e480f43c22506092d3a4b5fc2235321ad90c6f684d6574616461746142a0126850726f76696465727834313244334b6f6f57507154326e4d44536958555378354437666173617868784b696756686371666b4b71724c67684371396a787a694164647265737365738178252f646e73342f697066732e656e636c6176652e686f73742f7463702f3434332f687474707369436f6e74657874494451736974652d726f6f742d636f6e74657874695369676e617475726558a90a2408011220d04ab232742bb4ab3a1368bd4615e4e6d0224ab71a016baf8520a332c9778737121b2f696e64657865722f696e676573742f61645369676e61747572651a2212202a6f431ba40eef8acea3685870baff60237c2179880f41a9880b11f50c6af9ff2a4027688c4311d5937c397177386ed775a9a77000cb0e62237dcf8e309c3565a9b26670b3b350af44557ac9047da287d347f178acc382b357619f50884c7e4114026a50726576696f75734944d82a58250001711220feda3d2c525118459cdfdebed233186f14d38a8bb0d8a8901b0dc264f35d93ed";
    const AD2_CID: &str = "bafyreig25m5hqbjutuog6ggl2uzg4qqfakfgrl2sar247slbhw2bs73s5i";
    const SIGNEDHEAD_DAGJSON: &str = "7b2268656164223a7b222f223a22626166797265696732356d356871626a7574756f673667676c32757a6734717166616b6667726c32736172323437736c6268773262733733733569227d2c227075626b6579223a7b222f223a7b226279746573223a2243414553494e424b736a4a304b3753724f684e6f76555956354f6251496b713347674672723455676f7a4c4a64346333227d7d2c22736967223a7b222f223a7b226279746573223a226178507a507a304a704244424d59314d43315056466555716e684e7a49624e50306359484e3178567367494c4c794732383339586667644a2f6d65704844616775702b6d4c38473166465271665a6873736138774241227d7d7d";

    fn key() -> SigningKey {
        SigningKey::from_bytes(&SEED)
    }
    fn ctx() -> Vec<u8> {
        b"site-root-context".to_vec()
    }
    fn addrs() -> Vec<String> {
        vec!["/dns4/ipfs.enclave.host/tcp/443/https".to_string()]
    }

    #[test]
    fn entry_chunk_matches_go() {
        let mhs = vec![hex_decode(MH1).unwrap(), hex_decode(MH2).unwrap()];
        let block = entry_chunk(&mhs);
        assert_eq!(hex(&block), ENTRYCHUNK_DAGCBOR);
        assert_eq!(cid_string(&cid_v1(CODEC_DAG_CBOR, &block)), ENTRYCHUNK_CID);
    }

    #[test]
    fn advertisement_and_signature_match_go() {
        let ec_cid = cid_v1(CODEC_DAG_CBOR, &entry_chunk(&[hex_decode(MH1).unwrap(), hex_decode(MH2).unwrap()]));
        let addrs = addrs();
        let ctx = ctx();
        let ad = Advertisement {
            previous: None,
            provider: PROVIDER,
            addresses: &addrs,
            entries: &ec_cid,
            context_id: &ctx,
            metadata: &METADATA_HTTP_GATEWAY,
            is_rm: false,
        };
        let sig = ad.sign(&key());
        assert_eq!(hex(&sig), AD_SIGNATURE, "signature envelope");
        let (block, cid) = ad.build(&key());
        assert_eq!(hex(&block), AD_DAGCBOR, "ad dag-cbor");
        assert_eq!(cid_string(&cid), AD_CID, "ad cid");
    }

    #[test]
    fn chained_advertisement_matches_go() {
        let ec_cid = cid_v1(CODEC_DAG_CBOR, &entry_chunk(&[hex_decode(MH1).unwrap(), hex_decode(MH2).unwrap()]));
        let ad1_cid = hex_decode(&{
            // recompute ad1 cid bytes from its block
            let a = Advertisement { previous: None, provider: PROVIDER, addresses: &addrs(), entries: &ec_cid, context_id: &ctx(), metadata: &METADATA_HTTP_GATEWAY, is_rm: false };
            hex(&a.build(&key()).1)
        }).unwrap();
        let addrs = addrs();
        let ctx = ctx();
        let ad2 = Advertisement {
            previous: Some(&ad1_cid),
            provider: PROVIDER,
            addresses: &addrs,
            entries: &ec_cid,
            context_id: &ctx,
            metadata: &METADATA_HTTP_GATEWAY,
            is_rm: false,
        };
        let (block, cid) = ad2.build(&key());
        assert_eq!(hex(&block), AD2_DAGCBOR, "ad2 dag-cbor");
        assert_eq!(cid_string(&cid), AD2_CID, "ad2 cid");
    }

    #[test]
    fn announce_message_matches_go() {
        const ANNOUNCE_CBOR: &str = "83d82a58250001711220daeb3a7805349d1c6f18cbd5326e4205028a68af520475cfc9613db4197f72ea8152360b7075622e6578616d706c650601bbbb0340";
        let head = cid_bytes_of(AD2_CID);
        let addr = crate::multiformats::multiaddr_encode("/dns4/pub.example/tcp/443/https").unwrap();
        let msg = announce_message(&head, &[addr]);
        assert_eq!(hex(&msg), ANNOUNCE_CBOR);
    }

    #[test]
    fn signed_head_matches_go() {
        let ad2_cid = cid_bytes_of(AD2_CID);
        let json = signed_head(&ad2_cid, None, &key());
        let want = String::from_utf8(hex_decode(SIGNEDHEAD_DAGJSON).unwrap()).unwrap();
        assert_eq!(json, want);
    }

    // helper: decode a base32 CID string back to bytes
    fn cid_bytes_of(s: &str) -> Vec<u8> {
        crate::multiformats::base32_decode(&s[1..]).unwrap()
    }
}
