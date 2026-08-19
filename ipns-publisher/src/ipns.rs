//! IPNS records and the identities around them, hand-rolled from
//! specs.ipfs.tech/ipns/ipns-record and verified byte-for-byte against kubo
//! 0.42 (see the unit tests: the record vector was minted by `ipfs name
//! publish` and re-derived here from the same key and fields).
//!
//! The pieces:
//! - libp2p PublicKey/PrivateKey protobufs and the ed25519 identity chain:
//!   pubkey -> identity multihash -> peer ID (base58) -> IPNS name (CIDv1
//!   base36, codec libp2p-key) -> DHT routing key (b"/ipns/" + multihash).
//! - IpnsEntry: V2 signature over b"ipns-signature:" + DAG-CBOR data (keys
//!   canonically ordered TTL/Value/Sequence/Validity/ValidityType), V1
//!   signature kept for back-compat, field-1..6 mirrors of the CBOR fields
//!   (verifiers check the two agree).

#![allow(dead_code)]

use crate::multiformats::{
    base36, base58btc, base64_decode, hex_decode, varint, varint_read,
};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};

pub const MAX_RECORD_BYTES: usize = 10 * 1024; // spec: records over 10 KiB are rejected

// ---- protobuf primitives ---------------------------------------------------

pub fn pb_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    varint(out, field << 3 | 2);
    varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

pub fn pb_uint(out: &mut Vec<u8>, field: u64, v: u64) {
    varint(out, field << 3);
    varint(out, v);
}

/// Walk a protobuf message, calling `f(field, wire_type, varint_or_len, payload)`.
/// Returns None on malformed input. Only wire types 0 (varint) and 2 (len)
/// appear in the messages this app speaks.
pub fn pb_scan(mut buf: &[u8], mut f: impl FnMut(u64, u64, &[u8])) -> Option<()> {
    while !buf.is_empty() {
        let (key, n) = varint_read(buf)?;
        buf = &buf[n..];
        let (field, wire) = (key >> 3, key & 7);
        match wire {
            0 => {
                let (v, n) = varint_read(buf)?;
                buf = &buf[n..];
                let tmp = v.to_le_bytes();
                f(field, 0, &tmp[..]);
            }
            2 => {
                let (len, n) = varint_read(buf)?;
                buf = &buf[n..];
                if buf.len() < len as usize {
                    return None;
                }
                f(field, 2, &buf[..len as usize]);
                buf = &buf[len as usize..];
            }
            _ => return None,
        }
    }
    Some(())
}

fn pb_varint_at(buf: &[u8]) -> u64 {
    u64::from_le_bytes(buf.try_into().unwrap_or([0; 8]))
}

// ---- identity --------------------------------------------------------------

pub struct Identity {
    pub signing: SigningKey,
    pub pubkey_pb: Vec<u8>,   // libp2p PublicKey protobuf {Ed25519, 32B}
    pub peer_mh: Vec<u8>,     // identity multihash of pubkey_pb (0x00, len, bytes)
}

impl Identity {
    pub fn from_seed(seed: [u8; 32]) -> Identity {
        let signing = SigningKey::from_bytes(&seed);
        let pubkey_pb = pubkey_protobuf(signing.verifying_key().as_bytes());
        let peer_mh = identity_multihash(&pubkey_pb);
        Identity { signing, pubkey_pb, peer_mh }
    }

    /// Parse the configured key. Accepts hex or base64 of: a 32-byte seed,
    /// a 64-byte seed||pub, or the libp2p PrivateKey protobuf that
    /// `ipfs key export` writes ({Type: Ed25519, Data: 64B}).
    pub fn parse(s: &str) -> Result<Identity, String> {
        let s = s.trim();
        let bytes = hex_decode(s)
            .or_else(|| base64_decode(s))
            .ok_or("ipnsKey is neither hex nor base64")?;
        let seed: [u8; 32] = match bytes.len() {
            32 => bytes[..].try_into().unwrap(),
            64 => bytes[..32].try_into().unwrap(),
            _ => {
                // libp2p PrivateKey protobuf: Type(1)=Ed25519(1), Data(2)
                let mut ktype = u64::MAX;
                let mut data: Vec<u8> = Vec::new();
                pb_scan(&bytes, |field, wire, payload| match (field, wire) {
                    (1, 0) => ktype = pb_varint_at(payload),
                    (2, 2) => data = payload.to_vec(),
                    _ => {}
                })
                .ok_or("ipnsKey: unrecognized key container")?;
                if ktype != 1 {
                    return Err(format!(
                        "ipnsKey: key type {ktype} is not ed25519 (only ed25519 names are supported)"
                    ));
                }
                match data.len() {
                    64 | 32 => data[..32].try_into().unwrap(),
                    n => return Err(format!("ipnsKey: ed25519 key data is {n} bytes, want 32 or 64")),
                }
            }
        };
        let id = Identity::from_seed(seed);
        // a 64-byte form carries the public half: verify it matches
        if bytes.len() == 64 && bytes[32..] != *id.signing.verifying_key().as_bytes() {
            return Err("ipnsKey: public half does not match the seed".into());
        }
        Ok(id)
    }

    /// The 12D3Koo… form.
    pub fn peer_id(&self) -> String {
        base58btc(&self.peer_mh)
    }

    /// The k51… form: CIDv1 {libp2p-key} of the identity multihash, base36.
    pub fn ipns_name(&self) -> String {
        ipns_name_of(&self.peer_mh)
    }

    /// The DHT key the record is stored under: b"/ipns/" + multihash bytes.
    pub fn routing_key(&self) -> Vec<u8> {
        routing_key_of(&self.peer_mh)
    }
}

pub fn pubkey_protobuf(pubkey: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(36);
    pb_uint(&mut out, 1, 1); // Type = Ed25519
    pb_bytes(&mut out, 2, pubkey);
    out
}

/// Identity multihash (code 0x00) — ed25519 public key protobufs are 36
/// bytes, under the 42-byte inlining threshold, so peer IDs embed the key.
pub fn identity_multihash(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 2);
    varint(&mut out, 0x00);
    varint(&mut out, data.len() as u64);
    out.extend_from_slice(data);
    out
}

pub fn ipns_name_of(peer_mh: &[u8]) -> String {
    let mut cid = Vec::with_capacity(peer_mh.len() + 2);
    cid.push(0x01); // CIDv1
    varint(&mut cid, 0x72); // libp2p-key
    cid.extend_from_slice(peer_mh);
    format!("k{}", base36(&cid))
}

pub fn routing_key_of(peer_mh: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(6 + peer_mh.len());
    key.extend_from_slice(b"/ipns/");
    key.extend_from_slice(peer_mh);
    key
}

/// Extract the ed25519 public key from an identity-multihash peer ID, when
/// it is one (12D3Koo… peers). Qm… peers hash their key instead.
pub fn peer_mh_pubkey(peer_mh: &[u8]) -> Option<[u8; 32]> {
    let (code, n) = varint_read(peer_mh)?;
    if code != 0x00 {
        return None;
    }
    let (len, m) = varint_read(&peer_mh[n..])?;
    let pb = &peer_mh[n + m..];
    if pb.len() != len as usize {
        return None;
    }
    let mut ktype = u64::MAX;
    let mut data: Vec<u8> = Vec::new();
    pb_scan(pb, |field, wire, payload| match (field, wire) {
        (1, 0) => ktype = pb_varint_at(payload),
        (2, 2) => data = payload.to_vec(),
        _ => {}
    })?;
    if ktype != 1 || data.len() != 32 {
        return None;
    }
    data.try_into().ok()
}

// ---- DAG-CBOR data ---------------------------------------------------------

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

/// The IPNS data map, keys in canonical order (length, then bytewise):
/// TTL, Value, Sequence, Validity, ValidityType.
fn cbor_data(value: &[u8], validity: &[u8], sequence: u64, ttl: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + value.len() + validity.len());
    out.push(0xa5); // map(5)
    cbor_text(&mut out, "TTL");
    cbor_uint(&mut out, ttl);
    cbor_text(&mut out, "Value");
    cbor_bytes(&mut out, value);
    cbor_text(&mut out, "Sequence");
    cbor_uint(&mut out, sequence);
    cbor_text(&mut out, "Validity");
    cbor_bytes(&mut out, validity);
    cbor_text(&mut out, "ValidityType");
    cbor_uint(&mut out, 0); // EOL
    out
}

// ---- record build / parse / verify ----------------------------------------

pub struct Record {
    pub value: Vec<u8>,
    pub validity: Vec<u8>, // RFC3339 bytes
    pub sequence: u64,
    pub ttl: u64, // nanoseconds
}

/// Build and sign a V1+V2 IpnsEntry. `validity` is the RFC3339 EOL string.
pub fn build_record(id: &Identity, value: &[u8], validity: &str, sequence: u64, ttl_ns: u64) -> Result<Vec<u8>, String> {
    let data = cbor_data(value, validity.as_bytes(), sequence, ttl_ns);
    let mut v2_msg = Vec::with_capacity(15 + data.len());
    v2_msg.extend_from_slice(b"ipns-signature:");
    v2_msg.extend_from_slice(&data);
    let sig_v2 = id.signing.sign(&v2_msg).to_bytes();
    // V1: sign(value || validity || "EOL")
    let mut v1_msg = Vec::with_capacity(value.len() + validity.len() + 3);
    v1_msg.extend_from_slice(value);
    v1_msg.extend_from_slice(validity.as_bytes());
    v1_msg.extend_from_slice(b"EOL");
    let sig_v1 = id.signing.sign(&v1_msg).to_bytes();

    let mut out = Vec::with_capacity(200 + data.len());
    pb_bytes(&mut out, 1, value);
    pb_bytes(&mut out, 2, &sig_v1);
    pb_uint(&mut out, 3, 0); // validityType = EOL
    pb_bytes(&mut out, 4, validity.as_bytes());
    pb_uint(&mut out, 5, sequence);
    pb_uint(&mut out, 6, ttl_ns);
    // field 7 (pubKey) omitted: ed25519 peer IDs embed the key
    pb_bytes(&mut out, 8, &sig_v2);
    pb_bytes(&mut out, 9, &data);
    if out.len() > MAX_RECORD_BYTES {
        return Err(format!("record is {} bytes, the spec caps it at {}", out.len(), MAX_RECORD_BYTES));
    }
    Ok(out)
}

/// Parse an IpnsEntry's wire fields (unverified).
pub fn parse_record(bytes: &[u8]) -> Option<(Record, Vec<u8>, Vec<u8>)> {
    let mut value = Vec::new();
    let mut validity = Vec::new();
    let mut sequence = 0u64;
    let mut ttl = 0u64;
    let mut sig_v2 = Vec::new();
    let mut data = Vec::new();
    pb_scan(bytes, |field, wire, payload| match (field, wire) {
        (1, 2) => value = payload.to_vec(),
        (4, 2) => validity = payload.to_vec(),
        (5, 0) => sequence = pb_varint_at(payload),
        (6, 0) => ttl = pb_varint_at(payload),
        (8, 2) => sig_v2 = payload.to_vec(),
        (9, 2) => data = payload.to_vec(),
        _ => {}
    })?;
    Some((Record { value, validity, sequence, ttl }, sig_v2, data))
}

/// Verify a record against a public key: V2 signature over the CBOR data,
/// and the CBOR fields (the source of truth) parsed out of it. Returns the
/// record as the CBOR asserts it. Used for GET_VALUE responses (sequence
/// recovery and read-back), so it must not trust the protobuf mirrors.
pub fn verify_record(bytes: &[u8], pubkey: &[u8; 32]) -> Result<Record, String> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("record exceeds 10 KiB".into());
    }
    let (_, sig_v2, data) = parse_record(bytes).ok_or("malformed IpnsEntry protobuf")?;
    if sig_v2.len() != 64 {
        return Err("missing/short signatureV2".into());
    }
    let vk = VerifyingKey::from_bytes(pubkey).map_err(|e| format!("bad pubkey: {e}"))?;
    let mut msg = Vec::with_capacity(15 + data.len());
    msg.extend_from_slice(b"ipns-signature:");
    msg.extend_from_slice(&data);
    let sig = ed25519_dalek::Signature::from_bytes(sig_v2[..].try_into().unwrap());
    vk.verify(&msg, &sig).map_err(|_| "signatureV2 verification failed")?;
    cbor_data_fields(&data)
}

/// Pull Value/Validity/Sequence/TTL out of the signed CBOR map.
fn cbor_data_fields(mut b: &[u8]) -> Result<Record, String> {
    fn head(b: &mut &[u8]) -> Option<(u8, u64)> {
        let first = *b.first()?;
        let (major, info) = (first >> 5, first & 0x1f);
        let (v, adv) = match info {
            0..=23 => (u64::from(info), 1),
            24 => (u64::from(*b.get(1)?), 2),
            25 => (u64::from(u16::from_be_bytes([*b.get(1)?, *b.get(2)?])), 3),
            26 => {
                let mut a = [0u8; 4];
                for (i, x) in a.iter_mut().enumerate() {
                    *x = *b.get(1 + i)?;
                }
                (u64::from(u32::from_be_bytes(a)), 5)
            }
            27 => {
                let mut a = [0u8; 8];
                for (i, x) in a.iter_mut().enumerate() {
                    *x = *b.get(1 + i)?;
                }
                (u64::from_be_bytes(a), 9)
            }
            _ => return None,
        };
        *b = &b[adv..];
        Some((major, v))
    }
    let (major, n) = head(&mut b).ok_or("cbor: truncated")?;
    if major != 5 {
        return Err("cbor: data is not a map".into());
    }
    let mut rec = Record { value: Vec::new(), validity: Vec::new(), sequence: 0, ttl: 0 };
    for _ in 0..n {
        let (kmaj, klen) = head(&mut b).ok_or("cbor: truncated key")?;
        if kmaj != 3 || b.len() < klen as usize {
            return Err("cbor: bad key".into());
        }
        let key = std::str::from_utf8(&b[..klen as usize]).map_err(|_| "cbor: key utf8")?.to_string();
        b = &b[klen as usize..];
        let (vmaj, v) = head(&mut b).ok_or("cbor: truncated value")?;
        match (key.as_str(), vmaj) {
            ("TTL", 0) => rec.ttl = v,
            ("Sequence", 0) => rec.sequence = v,
            ("ValidityType", 0) => {
                if v != 0 {
                    return Err(format!("cbor: unsupported ValidityType {v}"));
                }
            }
            ("Value", 2) | ("Validity", 2) => {
                if b.len() < v as usize {
                    return Err("cbor: truncated bytes".into());
                }
                let bytes = b[..v as usize].to_vec();
                b = &b[v as usize..];
                if key == "Value" {
                    rec.value = bytes;
                } else {
                    rec.validity = bytes;
                }
            }
            _ => return Err(format!("cbor: unexpected entry {key}/{vmaj}")),
        }
    }
    Ok(rec)
}

/// Is this EOL still in the future? Parses the RFC3339 validity into unix
/// seconds (fractional part ignored: a boundary this close is expired).
pub fn validity_unix(validity: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(validity).ok()?;
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days-from-civil (Howard Hinnant)
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// RFC3339 UTC at whole-second precision from unix seconds (the shape Go's
/// RFC3339Nano prints when nanos are zero, which kubo parses fine).
pub fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86400);
    let sod = unix.rem_euclid(86400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiformats::hex;

    // Vectors minted by kubo 0.42: `ipfs key gen --type=ed25519 t1`,
    // `ipfs key export`, `ipfs name publish --allow-offline --lifetime=48h
    // --ttl=17m /ipfs/bafkreif…`, record fetched over /routing/v1/ipns/.
    const SEED: &str = "32e1a1eb35a22c55220781fd739bbdab97470ea5a5873b7a0ad33f20182316cc";
    const PUB: &str = "a86e2476a314c6e8b1eaa60a0934cce962521ec6b25b9ed9e666f7c26609b877";
    const PEER_ID: &str = "12D3KooWM9r5AMSSKjFBSjD9VAJhP28VkqtMYZ2tEax4hbvdvQAA";
    const NAME: &str = "k51qzi5uqu5dkdpqzhsh9vvcrli4w59y75nr6n59rc7bzgfz6zzk4zhdj6huuv";
    const RECORD: &str = "0a412f697066732f6261666b726569667a6a7574337465326e6879656b6b6c737332376e68336b37327973636f377933326b6f616f356565693636776f6633366e35651240515a665804e39917ceac635e48f239d7960b8a4c874fbf84f6b7d95b524349b227cd2bbc0a741e02ad9f31901831f59f706f70b27a205d20679aa296f8cfb6041800221e323032362d30382d32315430363a30393a30342e3531353939393530325a28003080b0f3e5d71d424097d9301110f31d57b65395f3ab5004086edce1b6955660d6afedf9ea449472993f18b59cc3bb89ddf6929243fe2f6df734e8191006c57c36b9cbab963565a70d4a9801a56354544c1b000000ed7cbcd8006556616c756558412f697066732f6261666b726569667a6a7574337465326e6879656b6b6c737332376e68336b37327973636f377933326b6f616f356565693636776f6633366e35656853657175656e6365006856616c6964697479581e323032362d30382d32315430363a30393a30342e3531353939393530325a6c56616c69646974795479706500";

    fn id() -> Identity {
        Identity::parse(SEED).unwrap()
    }

    #[test]
    fn identity_chain_matches_kubo() {
        let id = id();
        assert_eq!(hex(id.signing.verifying_key().as_bytes()), PUB);
        assert_eq!(id.peer_id(), PEER_ID);
        assert_eq!(id.ipns_name(), NAME);
        assert_eq!(&id.routing_key()[..6], b"/ipns/");
        assert_eq!(id.routing_key().len(), 6 + 38);
    }

    #[test]
    fn key_formats_parse_alike() {
        let seed_bytes = hex_decode(SEED).unwrap();
        let mut sp = seed_bytes.clone();
        sp.extend(hex_decode(PUB).unwrap());
        // protobuf {Type: Ed25519, Data: seed||pub} — what `ipfs key export` writes
        let mut pb = vec![0x08, 0x01, 0x12, 0x40];
        pb.extend_from_slice(&sp);
        for form in [
            SEED.to_string(),
            hex(&sp),
            hex(&pb),
            crate::multiformats::base64(&seed_bytes),
            crate::multiformats::base64(&pb),
        ] {
            assert_eq!(Identity::parse(&form).unwrap().peer_id(), PEER_ID, "form: {form}");
        }
        // corrupted public half must be refused
        sp[63] ^= 1;
        assert!(Identity::parse(&hex(&sp)).is_err());
    }

    #[test]
    fn record_bytes_match_kubo() {
        let kubo = hex_decode(RECORD).unwrap();
        let (rec, _, _) = parse_record(&kubo).unwrap();
        let mine = build_record(
            &id(),
            &rec.value,
            std::str::from_utf8(&rec.validity).unwrap(),
            rec.sequence,
            rec.ttl,
        )
        .unwrap();
        assert_eq!(hex(&mine), hex(&kubo));
    }

    #[test]
    fn verify_accepts_kubo_and_rejects_tampering() {
        let kubo = hex_decode(RECORD).unwrap();
        let pubkey: [u8; 32] = hex_decode(PUB).unwrap().try_into().unwrap();
        let rec = verify_record(&kubo, &pubkey).unwrap();
        assert_eq!(rec.sequence, 0);
        assert_eq!(rec.ttl, 1_020_000_000_000); // 17 minutes in ns
        assert_eq!(rec.value, b"/ipfs/bafkreifzjut3te2nhyekklss27nh3k72ysco7y32koao5eei66wof36n5e");
        // flip one CBOR byte: the V2 signature must fail
        let mut bad = kubo.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert!(verify_record(&bad, &pubkey).is_err());
        // wrong key
        let other: [u8; 32] = [9; 32];
        assert!(verify_record(&kubo, &other).is_err());
    }

    #[test]
    fn pubkey_recovered_from_peer_id() {
        let id = id();
        let pk = peer_mh_pubkey(&id.peer_mh).unwrap();
        assert_eq!(hex(&pk), PUB);
    }

    #[test]
    fn validity_roundtrip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_755_555_555), "2025-08-18T22:19:15Z");
        for t in [0i64, 951_827_696, 1_755_555_555, 4_102_444_800] {
            assert_eq!(validity_unix(rfc3339(t).as_bytes()), Some(t));
        }
        // kubo's nano-precision form parses to the whole second
        assert_eq!(
            validity_unix(b"2026-08-21T06:09:04.515999502Z"),
            validity_unix(b"2026-08-21T06:09:04Z")
        );
    }
}
