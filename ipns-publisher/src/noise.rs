//! Noise_XX_25519_ChaChaPoly_SHA256, the libp2p flavor: the handshake that
//! turns a raw TCP stream into an authenticated, encrypted channel bound to
//! a libp2p peer identity. Hand-rolled from the Noise spec + libp2p/noise;
//! the state machine is sans-io (the caller frames messages with the
//! 2-byte big-endian length prefix libp2p uses and moves the bytes).
//!
//! Identity binding: each side's handshake payload is a protobuf carrying
//! its libp2p PublicKey and an ed25519 signature over
//! b"noise-libp2p-static-key:" + <its x25519 static public key>. We verify
//! ed25519 remotes strictly; RSA/secp256k1 remotes get their peer ID
//! derived and compared but not signature-verified (a publisher's records
//! are self-certifying; see the README's trust notes).

#![allow(dead_code)]

use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::{AeadInPlace, ChaCha20Poly1305, KeyInit};
use ed25519_dalek::{Signer, Verifier};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as XPublic, StaticSecret};

use crate::ipns::{self, pb_bytes, pb_scan};

const PROTOCOL_NAME: &[u8; 32] = b"Noise_XX_25519_ChaChaPoly_SHA256";
const SIG_PREFIX: &[u8] = b"noise-libp2p-static-key:";
pub const TAG: usize = 16;
/// Max plaintext per transport message: 65535 minus the AEAD tag.
pub const MAX_PLAINTEXT: usize = 65535 - TAG;

fn hmac_sha256(key: &[u8], data: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    for d in data {
        mac.update(d);
    }
    mac.finalize().into_bytes().into()
}

/// Noise HKDF: returns (out1, out2).
fn hkdf2(ck: &[u8; 32], input: &[u8]) -> ([u8; 32], [u8; 32]) {
    let temp = hmac_sha256(ck, &[input]);
    let out1 = hmac_sha256(&temp, &[&[0x01]]);
    let out2 = hmac_sha256(&temp, &[&out1, &[0x02]]);
    (out1, out2)
}

// ---- cipher state ----------------------------------------------------------

pub struct CipherState {
    key: [u8; 32],
    nonce: u64,
}

impl CipherState {
    fn new(key: [u8; 32]) -> CipherState {
        CipherState { key, nonce: 0 }
    }

    fn nonce_bytes(&self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.nonce.to_le_bytes());
        n
    }

    /// Seal plaintext with the given associated data; bumps the nonce.
    pub fn seal(&mut self, ad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(&self.key));
        let mut buf = plaintext.to_vec();
        let tag = cipher
            .encrypt_in_place_detached(GenericArray::from_slice(&self.nonce_bytes()), ad, &mut buf)
            .expect("seal");
        self.nonce += 1;
        buf.extend_from_slice(&tag);
        buf
    }

    /// Open ciphertext+tag; bumps the nonce only on success.
    pub fn open(&mut self, ad: &[u8], ct: &[u8]) -> Result<Vec<u8>, String> {
        if ct.len() < TAG {
            return Err("noise: ciphertext shorter than the tag".into());
        }
        let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(&self.key));
        let (body, tag) = ct.split_at(ct.len() - TAG);
        let mut buf = body.to_vec();
        cipher
            .decrypt_in_place_detached(
                GenericArray::from_slice(&self.nonce_bytes()),
                ad,
                &mut buf,
                GenericArray::from_slice(tag),
            )
            .map_err(|_| "noise: decrypt failed".to_string())?;
        self.nonce += 1;
        Ok(buf)
    }
}

// ---- symmetric state -------------------------------------------------------

struct Symmetric {
    ck: [u8; 32],
    h: [u8; 32],
    cipher: Option<CipherState>,
}

impl Symmetric {
    fn new() -> Symmetric {
        // the protocol name is exactly 32 bytes, so h starts as the name
        let h = *PROTOCOL_NAME;
        let mut s = Symmetric { ck: h, h, cipher: None };
        s.mix_hash(&[]); // empty prologue
        s
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut hasher = Sha256::new();
        hasher.update(self.h);
        hasher.update(data);
        self.h = hasher.finalize().into();
    }

    fn mix_key(&mut self, input: &[u8]) {
        let (ck, temp_k) = hkdf2(&self.ck, input);
        self.ck = ck;
        self.cipher = Some(CipherState::new(temp_k));
    }

    fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let out = match &mut self.cipher {
            Some(c) => c.seal(&self.h, plaintext),
            None => plaintext.to_vec(),
        };
        self.mix_hash(&out);
        out
    }

    fn decrypt_and_hash(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let out = match &mut self.cipher {
            Some(c) => c.open(&self.h, data)?,
            None => data.to_vec(),
        };
        self.mix_hash(data);
        Ok(out)
    }

    fn split(&self) -> (CipherState, CipherState) {
        let (k1, k2) = hkdf2(&self.ck, &[]);
        (CipherState::new(k1), CipherState::new(k2))
    }
}

// ---- handshake payload -----------------------------------------------------

pub struct RemoteIdentity {
    pub peer_mh: Vec<u8>,        // multihash peer id derived from the key
    pub pubkey_pb: Vec<u8>,      // libp2p PublicKey protobuf
    pub key_type: u64,           // 0 RSA, 1 Ed25519, 2 Secp256k1, 3 ECDSA
    pub sig_verified: bool,      // ed25519 only; others ride unverified
}

/// Peer ID from a PublicKey protobuf: identity multihash when the protobuf
/// is 42 bytes or shorter (ed25519, secp256k1), sha2-256 otherwise (RSA).
pub fn peer_mh_of_pubkey(pubkey_pb: &[u8]) -> Vec<u8> {
    if pubkey_pb.len() <= 42 {
        ipns::identity_multihash(pubkey_pb)
    } else {
        let digest: [u8; 32] = Sha256::digest(pubkey_pb).into();
        let mut mh = Vec::with_capacity(34);
        mh.push(0x12);
        mh.push(0x20);
        mh.extend_from_slice(&digest);
        mh
    }
}

fn build_payload(identity: &ed25519_dalek::SigningKey, static_pub: &XPublic) -> Vec<u8> {
    let pubkey_pb = ipns::pubkey_protobuf(identity.verifying_key().as_bytes());
    let mut msg = Vec::with_capacity(SIG_PREFIX.len() + 32);
    msg.extend_from_slice(SIG_PREFIX);
    msg.extend_from_slice(static_pub.as_bytes());
    let sig = identity.sign(&msg).to_bytes();
    let mut out = Vec::with_capacity(pubkey_pb.len() + 64 + 8);
    pb_bytes(&mut out, 1, &pubkey_pb);
    pb_bytes(&mut out, 2, &sig);
    out
}

fn parse_payload(payload: &[u8], remote_static: &[u8; 32]) -> Result<RemoteIdentity, String> {
    let mut identity_key: Vec<u8> = Vec::new();
    let mut identity_sig: Vec<u8> = Vec::new();
    pb_scan(payload, |field, wire, data| match (field, wire) {
        (1, 2) => identity_key = data.to_vec(),
        (2, 2) => identity_sig = data.to_vec(),
        _ => {}
    })
    .ok_or("noise payload: malformed protobuf")?;
    if identity_key.is_empty() {
        return Err("noise payload: no identity key".into());
    }
    let mut key_type = 0u64;
    let mut key_data: Vec<u8> = Vec::new();
    pb_scan(&identity_key, |field, wire, data| match (field, wire) {
        (1, 0) => key_type = u64::from_le_bytes(data.try_into().unwrap_or([0; 8])),
        (2, 2) => key_data = data.to_vec(),
        _ => {}
    })
    .ok_or("noise payload: malformed PublicKey")?;
    let mut msg = Vec::with_capacity(SIG_PREFIX.len() + 32);
    msg.extend_from_slice(SIG_PREFIX);
    msg.extend_from_slice(remote_static);
    let sig_verified = if key_type == 1 {
        let key: [u8; 32] = key_data
            .as_slice()
            .try_into()
            .map_err(|_| "noise payload: ed25519 key is not 32 bytes")?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&key)
            .map_err(|e| format!("noise payload: bad ed25519 key: {e}"))?;
        let sig: [u8; 64] = identity_sig
            .as_slice()
            .try_into()
            .map_err(|_| "noise payload: ed25519 sig is not 64 bytes")?;
        vk.verify(&msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .map_err(|_| "noise payload: identity signature failed")?;
        true
    } else {
        false // RSA/secp256k1/ECDSA peers: identity rides unverified
    };
    Ok(RemoteIdentity {
        peer_mh: peer_mh_of_pubkey(&identity_key),
        pubkey_pb: identity_key,
        key_type,
        sig_verified,
    })
}

// ---- XX handshake (initiator) ----------------------------------------------

fn fresh_x25519() -> StaticSecret {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("wasi random");
    StaticSecret::from(bytes)
}

/// Initiator-side XX. new() emits message A; read_b() consumes message B
/// and emits message C plus the transport ciphers and the remote identity.
pub struct Handshake {
    sym: Symmetric,
    e: StaticSecret,
    s: StaticSecret,
    identity: ed25519_dalek::SigningKey,
}

pub struct Established {
    pub send: CipherState,
    pub recv: CipherState,
    pub remote: RemoteIdentity,
}

impl Handshake {
    /// Start the handshake; returns the state and message A's body
    /// (the caller adds the 2-byte length frame).
    pub fn start(identity: ed25519_dalek::SigningKey) -> (Handshake, Vec<u8>) {
        let mut sym = Symmetric::new();
        let e = fresh_x25519();
        let e_pub = XPublic::from(&e);
        sym.mix_hash(e_pub.as_bytes());
        let payload = sym.encrypt_and_hash(&[]); // empty payload, pre-key: plaintext
        let mut msg = Vec::with_capacity(32 + payload.len());
        msg.extend_from_slice(e_pub.as_bytes());
        msg.extend_from_slice(&payload);
        (Handshake { sym, e, s: fresh_x25519(), identity }, msg)
    }

    /// Consume message B (e, ee, s, es, payload), produce message C
    /// (s, se, payload) and the split transport ciphers.
    pub fn read_b(mut self, msg_b: &[u8]) -> Result<(Vec<u8>, Established), String> {
        if msg_b.len() < 32 + 48 + TAG {
            return Err(format!("noise: message B too short ({} bytes)", msg_b.len()));
        }
        // e
        let re: [u8; 32] = msg_b[..32].try_into().unwrap();
        self.sym.mix_hash(&re);
        let re_pub = XPublic::from(re);
        // ee
        self.sym.mix_key(self.e.diffie_hellman(&re_pub).as_bytes());
        // s (encrypted, 48 bytes)
        let rs_ct = &msg_b[32..80];
        let rs_bytes = self.sym.decrypt_and_hash(rs_ct)?;
        let rs: [u8; 32] = rs_bytes.as_slice().try_into().map_err(|_| "noise: bad rs")?;
        let rs_pub = XPublic::from(rs);
        // es (initiator: DH(e, rs))
        self.sym.mix_key(self.e.diffie_hellman(&rs_pub).as_bytes());
        // payload
        let remote_payload = self.sym.decrypt_and_hash(&msg_b[80..])?;
        let remote = parse_payload(&remote_payload, &rs)?;

        // message C: s, se, payload
        let s_pub = XPublic::from(&self.s);
        let mut msg_c = self.sym.encrypt_and_hash(s_pub.as_bytes());
        // se (initiator: DH(s, re))
        self.sym.mix_key(self.s.diffie_hellman(&re_pub).as_bytes());
        let payload = build_payload(&self.identity, &s_pub);
        msg_c.extend_from_slice(&self.sym.encrypt_and_hash(&payload));

        let (send, recv) = self.sym.split();
        Ok((msg_c, Established { send, recv, remote }))
    }
}

// ---- responder (tests only: proves the state machine against itself) -------

#[cfg(test)]
pub struct Responder {
    sym: Symmetric,
    e: StaticSecret,
    s: StaticSecret,
    re: Option<XPublic>,
    identity: ed25519_dalek::SigningKey,
}

#[cfg(test)]
impl Responder {
    pub fn new(identity: ed25519_dalek::SigningKey) -> Responder {
        Responder { sym: Symmetric::new(), e: fresh_x25519(), s: fresh_x25519(), re: None, identity }
    }

    pub fn read_a_write_b(&mut self, msg_a: &[u8]) -> Result<Vec<u8>, String> {
        if msg_a.len() < 32 {
            return Err("short A".into());
        }
        let re: [u8; 32] = msg_a[..32].try_into().unwrap();
        self.sym.mix_hash(&re);
        let re_pub = XPublic::from(re);
        self.sym.decrypt_and_hash(&msg_a[32..])?; // empty payload
        self.re = Some(re_pub);
        // B: e, ee, s, es, payload
        let e_pub = XPublic::from(&self.e);
        let mut out = e_pub.as_bytes().to_vec();
        self.sym.mix_hash(e_pub.as_bytes());
        self.sym.mix_key(self.e.diffie_hellman(&re_pub).as_bytes()); // ee
        let s_pub = XPublic::from(&self.s);
        out.extend_from_slice(&self.sym.encrypt_and_hash(s_pub.as_bytes()));
        self.sym.mix_key(self.s.diffie_hellman(&re_pub).as_bytes()); // es (responder: DH(s, re))
        let payload = build_payload(&self.identity, &s_pub);
        out.extend_from_slice(&self.sym.encrypt_and_hash(&payload));
        Ok(out)
    }

    pub fn read_c(mut self) -> impl FnOnce(&[u8]) -> Result<(CipherState, CipherState, RemoteIdentity), String> {
        move |msg_c: &[u8]| {
            let rs_ct = msg_c.get(..48).ok_or("short C")?;
            let rs_bytes = self.sym.decrypt_and_hash(rs_ct)?;
            let rs: [u8; 32] = rs_bytes.as_slice().try_into().map_err(|_| "bad rs")?;
            let rs_pub = XPublic::from(rs);
            self.sym.mix_key(self.e.diffie_hellman(&rs_pub).as_bytes()); // se (responder: DH(e, rs))
            let payload = self.sym.decrypt_and_hash(&msg_c[48..])?;
            let remote = parse_payload(&payload, &rs)?;
            let (c1, c2) = self.sym.split();
            // responder: c1 receives (initiator sends on c1), c2 sends
            Ok((c2, c1, remote))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xx_handshake_against_own_responder() {
        let init_id = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
        let resp_id = ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]);
        let init_mh = peer_mh_of_pubkey(&ipns::pubkey_protobuf(init_id.verifying_key().as_bytes()));
        let resp_mh = peer_mh_of_pubkey(&ipns::pubkey_protobuf(resp_id.verifying_key().as_bytes()));

        let (hs, msg_a) = Handshake::start(init_id);
        let mut resp = Responder::new(resp_id);
        let msg_b = resp.read_a_write_b(&msg_a).unwrap();
        let (msg_c, mut est) = hs.read_b(&msg_b).unwrap();
        assert_eq!(est.remote.peer_mh, resp_mh);
        assert!(est.remote.sig_verified);
        let (mut r_send, mut r_recv, r_remote) = resp.read_c()(&msg_c).unwrap();
        assert_eq!(r_remote.peer_mh, init_mh);
        assert!(r_remote.sig_verified);

        // transport messages both ways
        let ct = est.send.seal(&[], b"hello from initiator");
        assert_eq!(r_recv.open(&[], &ct).unwrap(), b"hello from initiator");
        let ct = r_send.seal(&[], b"hello from responder");
        assert_eq!(est.recv.open(&[], &ct).unwrap(), b"hello from responder");
        // nonce advances: a replay must fail
        assert!(est.recv.open(&[], &ct).is_err());
    }

    #[test]
    fn tampered_b_fails() {
        let (hs, msg_a) = Handshake::start(ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]));
        let mut resp = Responder::new(ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]));
        let mut msg_b = resp.read_a_write_b(&msg_a).unwrap();
        let n = msg_b.len();
        msg_b[n - 1] ^= 1;
        assert!(hs.read_b(&msg_b).is_err());
    }
}
