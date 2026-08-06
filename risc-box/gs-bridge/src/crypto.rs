// AES helpers for the stream layer.
//
// The master key for every encrypted GameStream channel is the raw `rikey`
// bytes from /launch — no KDF, no transform (Sunshine src/nvhttp.cpp:368).

use openssl::symm::{Cipher, Crypter, Mode};

/// AES-128-GCM encrypt. Returns (tag, ciphertext). No AAD, no padding —
/// ciphertext length equals plaintext length.
pub fn gcm_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> ([u8; 16], Vec<u8>) {
    let mut c = Crypter::new(Cipher::aes_128_gcm(), Mode::Encrypt, key, Some(iv)).unwrap();
    c.pad(false);
    let mut out = vec![0u8; plaintext.len() + Cipher::aes_128_gcm().block_size()];
    let mut n = c.update(plaintext, &mut out).unwrap();
    n += c.finalize(&mut out[n..]).unwrap();
    out.truncate(n);
    let mut tag = [0u8; 16];
    c.get_tag(&mut tag).unwrap();
    (tag, out)
}

/// AES-128-GCM decrypt with an explicit tag. Returns None if authentication
/// fails — a bad tag means a forged or corrupt packet, never a usable message.
pub fn gcm_decrypt(key: &[u8], iv: &[u8], tag: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
    let mut c = Crypter::new(Cipher::aes_128_gcm(), Mode::Decrypt, key, Some(iv)).ok()?;
    c.pad(false);
    c.set_tag(tag).ok()?;
    let mut out = vec![0u8; ciphertext.len() + Cipher::aes_128_gcm().block_size()];
    let mut n = c.update(ciphertext, &mut out).ok()?;
    n += c.finalize(&mut out[n..]).ok()?;
    out.truncate(n);
    Some(out)
}

/// AES-128-CBC encrypt with PKCS#7 padding — the audio stream's cipher.
pub fn cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut c = Crypter::new(Cipher::aes_128_cbc(), Mode::Encrypt, key, Some(iv)).unwrap();
    c.pad(true);
    let mut out = vec![0u8; plaintext.len() + Cipher::aes_128_cbc().block_size() * 2];
    let mut n = c.update(plaintext, &mut out).unwrap();
    n += c.finalize(&mut out[n..]).unwrap();
    out.truncate(n);
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
