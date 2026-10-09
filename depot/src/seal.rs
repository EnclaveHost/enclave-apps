//! Encryption at rest. Everything the bucket holds is ChaCha20-Poly1305
//! under keys derived from the deployment's master secret:
//!
//! - small objects (the registry, each repository's manifest, each pack's
//!   index) are sealed whole: `DEP1 || nonce[12] || ciphertext+tag`, fresh
//!   random nonce, the object key as associated data;
//! - packs are sealed in fixed-size chunks so a fetch can range-read the
//!   part it needs. Chunk `i` is `ciphertext+tag` at `i * (CHUNK + 16)`,
//!   nonce = the chunk index, and the associated data binds the object key,
//!   the pack's total length, the chunk size and the index, so chunks
//!   cannot be reordered, truncated or moved to another pack. Each pack has
//!   its own key (derived from its random id), which is what makes a
//!   counter nonce safe.
//!
//! Whoever holds the master secret can decrypt everything; the bucket's
//! operator sees object sizes and counts, never names, refs or content.
//! Authentication detects alteration, not rollback to an older valid
//! object (the manifest's revision counter catches that within a process
//! lifetime only).

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const CHUNK: u32 = 256 * 1024;
pub const TAG: usize = 16;
const MAGIC: &[u8; 4] = b"DEP1";

#[derive(Clone)]
pub struct Keys {
    root: [u8; 32],
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

impl Keys {
    pub fn new(master: &str) -> Keys {
        Keys {
            root: hmac(b"depot-root-v1", &[master.as_bytes()]),
        }
    }
    pub fn derive(&self, scope: &str) -> [u8; 32] {
        hmac(&self.root, &[b"depot-v1:", scope.as_bytes()])
    }
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("the platform provides randomness");
    b
}

pub fn random_id() -> String {
    crate::git::hex(&random::<16>())
}

pub fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let c = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = random::<12>();
    let mut out = Vec::with_capacity(plain.len() + 32);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(plain);
    let tag = c
        .encrypt_in_place_detached(Nonce::from_slice(&nonce), aad, &mut out[16..])
        .expect("chacha20poly1305 seals any length we use");
    out.extend_from_slice(&tag);
    out
}

pub fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String> {
    if sealed.len() < 16 + TAG || &sealed[..4] != MAGIC {
        return Err("object is not sealed by depot".into());
    }
    let c = ChaCha20Poly1305::new(Key::from_slice(key));
    let (body, tag) = sealed[16..].split_at(sealed.len() - 16 - TAG);
    let mut buf = body.to_vec();
    c.decrypt_in_place_detached(
        Nonce::from_slice(&sealed[4..16]),
        aad,
        &mut buf,
        Tag::from_slice(tag),
    )
    .map_err(|_| {
        "cannot open a stored object: wrong master key, or the object was moved or altered"
            .to_string()
    })?;
    Ok(buf)
}

/// The chunked form of one pack object.
pub struct Chunked {
    cipher: ChaCha20Poly1305,
    object_key: String,
    pub total: u64,
    pub chunk: u32,
}

impl Chunked {
    pub fn new(key: &[u8; 32], object_key: &str, total: u64, chunk: u32) -> Chunked {
        Chunked {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            object_key: object_key.to_string(),
            total,
            chunk,
        }
    }
    pub fn chunks(&self) -> u64 {
        self.total.div_ceil(self.chunk as u64)
    }
    pub fn plain_len(&self, i: u64) -> u64 {
        (self.total - i * self.chunk as u64).min(self.chunk as u64)
    }
    pub fn cipher_offset(&self, i: u64) -> u64 {
        i * (self.chunk as u64 + TAG as u64)
    }
    pub fn cipher_total(&self) -> u64 {
        self.total + self.chunks() * TAG as u64
    }
    fn aad(&self, i: u64) -> Vec<u8> {
        let mut a = Vec::with_capacity(self.object_key.len() + 40);
        a.extend_from_slice(b"DEPK1");
        a.extend_from_slice(&(self.object_key.len() as u32).to_be_bytes());
        a.extend_from_slice(self.object_key.as_bytes());
        a.extend_from_slice(&self.total.to_be_bytes());
        a.extend_from_slice(&self.chunk.to_be_bytes());
        a.extend_from_slice(&i.to_be_bytes());
        a
    }
    fn nonce(i: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&i.to_be_bytes());
        n
    }
    /// Encrypt chunk `i` (exactly `plain_len(i)` bytes) in place; appends the tag.
    pub fn seal_chunk(&self, i: u64, buf: &mut Vec<u8>) {
        debug_assert_eq!(buf.len() as u64, self.plain_len(i));
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&Self::nonce(i)), &self.aad(i), buf)
            .expect("chunk seals");
        buf.extend_from_slice(&tag);
    }
    /// Decrypt one sealed chunk (`plain_len(i) + 16` bytes) in place.
    pub fn open_chunk(&self, i: u64, buf: &mut Vec<u8>) -> Result<(), String> {
        if i >= self.chunks() || buf.len() as u64 != self.plain_len(i) + TAG as u64 {
            return Err("stored pack chunk has the wrong length".into());
        }
        let t = buf.len() - TAG;
        let tag = *Tag::from_slice(&buf[t..]);
        buf.truncate(t);
        self.cipher
            .decrypt_in_place_detached(Nonce::from_slice(&Self::nonce(i)), &self.aad(i), buf, &tag)
            .map_err(|_| {
                "a stored pack chunk failed authentication (altered, moved, or wrong key)"
                    .to_string()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_objects_bind_key_and_name() {
        let k = Keys::new("a master secret of at least thirty-two chars");
        let a = k.derive("repo:1");
        let s = seal(&a, b"r/1/manifest", b"refs");
        assert_eq!(open(&a, b"r/1/manifest", &s).unwrap(), b"refs");
        assert!(open(&a, b"r/2/manifest", &s).is_err());
        assert!(open(&k.derive("repo:2"), b"r/1/manifest", &s).is_err());
        assert!(open(&Keys::new("other").derive("repo:1"), b"r/1/manifest", &s).is_err());
        let mut bad = s.clone();
        bad[20] ^= 1;
        assert!(open(&a, b"r/1/manifest", &bad).is_err());
        assert_ne!(s, seal(&a, b"r/1/manifest", b"refs"));
    }

    #[test]
    fn chunks_bind_position_and_length() {
        let key = Keys::new("m").derive("pack:x");
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let c = Chunked::new(&key, "r/1/p.pack", data.len() as u64, 300);
        assert_eq!(c.chunks(), 4);
        assert_eq!(c.plain_len(3), 100);
        let mut sealed = Vec::new();
        for i in 0..c.chunks() {
            let s = (i * 300) as usize;
            let mut b = data[s..s + c.plain_len(i) as usize].to_vec();
            c.seal_chunk(i, &mut b);
            assert_eq!(c.cipher_offset(i), sealed.len() as u64);
            sealed.extend_from_slice(&b);
        }
        assert_eq!(sealed.len() as u64, c.cipher_total());
        let mut c1 = sealed[316..632].to_vec();
        c.open_chunk(1, &mut c1).unwrap();
        assert_eq!(c1, &data[300..600]);
        let mut c1 = sealed[316..632].to_vec();
        assert!(c.open_chunk(2, &mut c1).is_err(), "moved chunk");
        let other = Chunked::new(&key, "r/1/p.pack", 1300, 300);
        let mut c0 = sealed[..316].to_vec();
        assert!(other.open_chunk(0, &mut c0).is_err(), "length is bound");
        let mut last = sealed[948..].to_vec();
        c.open_chunk(3, &mut last).unwrap();
    }
}
