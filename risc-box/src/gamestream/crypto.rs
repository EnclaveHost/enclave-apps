//! AES helpers for the stream layer, on pure-Rust primitives.
//!
//! The master key for every encrypted GameStream channel is the raw `rikey`
//! bytes from /launch — no KDF, no transform (Sunshine src/nvhttp.cpp:368).
//!
//! Ported from the native bridge's openssl `Crypter`: openssl is C and does not
//! build for wasm32-wasip2, so the in-guest host uses the RustCrypto stack.
//! Same modes, same framing, byte-for-byte compatible with the client — which
//! is the whole point, since Moonlight is the thing on the other end and it is
//! not changing.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Nonce};
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockEncryptMut, KeyIvInit};

type CbcEnc = cbc::Encryptor<aes::Aes128>;
type EcbEnc = ecb::Encryptor<aes::Aes128>;
type EcbDec = ecb::Decryptor<aes::Aes128>;

/// AES-128-GCM encrypt. Returns (tag, ciphertext). No AAD, no padding —
/// ciphertext length equals plaintext length.
///
/// `aes-gcm` returns ciphertext with the 16-byte tag appended; the wire format
/// carries them separately, so split rather than re-deriving.
pub fn gcm_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> ([u8; 16], Vec<u8>) {
    let c = Aes128Gcm::new(key.into());
    let mut out = c
        .encrypt(Nonce::from_slice(iv), Payload { msg: plaintext, aad: b"" })
        .expect("aes-128-gcm encrypt cannot fail on a valid key/nonce");
    let tag_at = out.len() - 16;
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&out[tag_at..]);
    out.truncate(tag_at);
    (tag, out)
}

/// AES-128-GCM decrypt with an explicit tag. Returns None if authentication
/// fails — a bad tag means a forged or corrupt packet, never a usable message.
pub fn gcm_decrypt(key: &[u8], iv: &[u8], tag: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
    if key.len() != 16 || iv.len() != 12 || tag.len() != 16 {
        return None;
    }
    let c = Aes128Gcm::new(key.into());
    let mut joined = Vec::with_capacity(ciphertext.len() + 16);
    joined.extend_from_slice(ciphertext);
    joined.extend_from_slice(tag);
    c.decrypt(Nonce::from_slice(iv), Payload { msg: &joined, aad: b"" }).ok()
}

/// AES-128-CBC encrypt with PKCS#7 padding — the audio stream's cipher.
pub fn cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Vec<u8> {
    CbcEnc::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

/// AES-128-ECB, one block at a time, no padding — the pairing handshake's
/// cipher. ECB is indefensible in general and correct here for exactly one
/// reason: it is what the GameStream pairing protocol specifies, and both ends
/// must agree. Everything it protects is a single random block.
pub fn ecb_encrypt_block(key: &[u8], block: &[u8]) -> Vec<u8> {
    use ecb::cipher::BlockEncryptMut as _;
    let mut out = block.to_vec();
    let mut enc = EcbEnc::new(key.into());
    for chunk in out.chunks_mut(16) {
        if chunk.len() == 16 {
            enc.encrypt_block_mut(chunk.into());
        }
    }
    out
}

/// The ECB inverse, for unwrapping what the client sent.
pub fn ecb_decrypt_block(key: &[u8], block: &[u8]) -> Vec<u8> {
    use ecb::cipher::BlockDecryptMut as _;
    let mut out = block.to_vec();
    let mut dec = EcbDec::new(key.into());
    for chunk in out.chunks_mut(16) {
        if chunk.len() == 16 {
            dec.decrypt_block_mut(chunk.into());
        }
    }
    out
}

/// The 12-byte control-stream IV: `[seq u32 LE][6 zero bytes][origin]['C']`.
///
/// `origin` is 'H' for host-originated and 'C' for client-originated; the two
/// directions keep independent sequence counters and never collide because of
/// that byte (moonlight-common-c ControlStream.c:552-630).
pub fn control_iv(seq: u32, host_originated: bool) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = if host_originated { b'H' } else { b'C' };
    iv[11] = b'C';
    iv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS-197 appendix C.1, the AES-128 known-answer vector. If the block
    /// cipher underneath is wrong, everything above it is wrong in a way that
    /// looks like "Moonlight just won't connect".
    #[test]
    fn aes128_matches_the_fips197_vector() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ];
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let want: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30,
            0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a,
        ];
        assert_eq!(ecb_encrypt_block(&key, &pt), want.to_vec());
        assert_eq!(ecb_decrypt_block(&key, &want), pt.to_vec());
    }

    /// GCM must round-trip and the ciphertext must be exactly as long as the
    /// plaintext — the wire format carries the tag separately, so a tag left
    /// glued on the end would desynchronise every packet after the first.
    #[test]
    fn gcm_round_trips_with_the_tag_split_off() {
        let key = [7u8; 16];
        let iv = control_iv(42, true);
        let msg = b"moonlight control payload";
        let (tag, ct) = gcm_encrypt(&key, &iv, msg);
        assert_eq!(ct.len(), msg.len(), "ciphertext must not carry the tag");
        assert_eq!(gcm_decrypt(&key, &iv, &tag, &ct).as_deref(), Some(&msg[..]));
    }

    /// A forged packet must not decrypt. This is the property that makes the
    /// control channel trustworthy at all.
    #[test]
    fn gcm_rejects_a_tampered_packet() {
        let key = [7u8; 16];
        let iv = control_iv(1, false);
        let (tag, mut ct) = gcm_encrypt(&key, &iv, b"input event");
        ct[0] ^= 0x01;
        assert!(gcm_decrypt(&key, &iv, &tag, &ct).is_none(), "a flipped bit must fail the tag");
        let (mut bad_tag, ct2) = gcm_encrypt(&key, &iv, b"input event");
        bad_tag[0] ^= 0x01;
        assert!(gcm_decrypt(&key, &iv, &bad_tag, &ct2).is_none(), "a flipped tag must fail");
    }

    /// The two directions must never share an IV even at the same sequence
    /// number — that byte is the only thing separating them.
    #[test]
    fn the_two_directions_get_different_ivs() {
        assert_ne!(control_iv(9, true), control_iv(9, false));
        assert_eq!(control_iv(9, true)[10], b'H');
        assert_eq!(control_iv(9, false)[10], b'C');
        assert_eq!(control_iv(0x01020304, true)[0..4], [0x04, 0x03, 0x02, 0x01]);
    }

    /// CBC pads to the block size; the audio path depends on that framing.
    #[test]
    fn cbc_pads_to_a_whole_block() {
        let out = cbc_encrypt(&[3u8; 16], &[4u8; 16], b"short");
        assert_eq!(out.len(), 16, "PKCS#7 must round 5 bytes up to one block");
        let exact = cbc_encrypt(&[3u8; 16], &[4u8; 16], &[0u8; 16]);
        assert_eq!(exact.len(), 32, "a full block gets a whole block of padding");
    }
}
