//! App-side snapshot envelope. S3 stores these bytes without knowing any keys.
//! Chunk authentication includes app, owner, bucket, object key, header, index
//! and length. An authenticated empty final record prevents prefix truncation.
//! This is confidentiality/integrity, not rollback protection or user login.
use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes256Gcm, Nonce, Tag,
};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use zeroize::{Zeroize, Zeroizing};

pub mod native;
pub mod policy;

const MAGIC: &[u8; 8] = b"EVMSNAP1";
const CHUNK: usize = 1024 * 1024;
const HEADER: usize = 40; // magic + random per-object key salt
const TAG: usize = 16;

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "snapshot authentication or format failed",
    )
}

/// Must come from trusted app configuration / verified identity, never a
/// request's unverified wallet field. Object identity uses length-prefixes.
pub struct Cipher {
    master: Zeroizing<[u8; 32]>,
    scope: [u8; 32],
}

impl Cipher {
    pub fn new(master: [u8; 32], app: &str, owner: &str, deployment: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"enclave-private-snapshot-v1");
        for part in [app, owner, deployment] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        Self {
            master: Zeroizing::new(master),
            scope: hash.finalize().into(),
        }
    }

    fn context(&self, bucket: &str, object: &str) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.scope);
        for part in [bucket, object] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        hash.finalize().into()
    }

    fn key(&self, context: &[u8; 32], header: &[u8; HEADER]) -> Aes256Gcm {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(self.master.as_ref()).expect("HMAC key");
        mac.update(context);
        mac.update(header);
        let mut key = mac.finalize().into_bytes();
        let cipher = Aes256Gcm::new(&key);
        key.as_mut_slice().zeroize();
        cipher
    }

    pub fn is_sealed(bytes: &[u8]) -> bool {
        bytes.starts_with(MAGIC)
    }

    /// Streams only ciphertext to `output`. A failed upload must not publish
    /// its partial output as a completed snapshot.
    pub fn seal(
        &self,
        bucket: &str,
        object: &str,
        mut input: impl Read,
        mut output: impl Write,
    ) -> io::Result<()> {
        let mut header = [0u8; HEADER];
        header[..8].copy_from_slice(MAGIC);
        OsRng
            .try_fill_bytes(&mut header[8..])
            .map_err(|_| io::Error::other("snapshot randomness unavailable"))?;
        let context = self.context(bucket, object);
        let cipher = self.key(&context, &header);
        output.write_all(&header)?;
        let mut buf = Zeroizing::new(vec![0u8; CHUNK]);
        let mut index = 0u64;
        loop {
            let mut len = 0;
            while len < CHUNK {
                match input.read(&mut buf[len..]) {
                    Ok(0) => break,
                    Ok(n) => len += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            let (nonce, aad) = record_context(&context, &header, index, len as u32);
            let tag = cipher
                .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut buf[..len])
                .map_err(|_| invalid())?;
            output.write_all(&(len as u32).to_be_bytes())?;
            output.write_all(&buf[..len])?;
            output.write_all(&tag)?;
            if len == 0 {
                return Ok(());
            }
            index = index.checked_add(1).ok_or_else(invalid)?;
        }
    }

    /// `output` MUST be private staging until this returns Ok: authenticated
    /// earlier chunks can have been written before a later chunk fails. Never
    /// feed partially authenticated state into a VM or publish a restore file.
    pub fn open(
        &self,
        bucket: &str,
        object: &str,
        mut input: impl Read,
        mut output: impl Write,
        limit: u64,
    ) -> io::Result<()> {
        let mut header = [0u8; HEADER];
        input.read_exact(&mut header)?;
        if &header[..8] != MAGIC {
            return Err(invalid());
        }
        let context = self.context(bucket, object);
        let cipher = self.key(&context, &header);
        let mut buf = Zeroizing::new(vec![0u8; CHUNK]);
        let mut index = 0u64;
        let mut total = 0u64;
        loop {
            let mut size = [0u8; 4];
            input.read_exact(&mut size)?;
            let len = u32::from_be_bytes(size) as usize;
            if len > CHUNK {
                return Err(invalid());
            }
            total = total
                .checked_add(len as u64)
                .filter(|n| *n <= limit)
                .ok_or_else(invalid)?;
            input.read_exact(&mut buf[..len])?;
            let mut tag = [0u8; TAG];
            input.read_exact(&mut tag)?;
            let (nonce, aad) = record_context(&context, &header, index, len as u32);
            cipher
                .decrypt_in_place_detached(
                    Nonce::from_slice(&nonce),
                    &aad,
                    &mut buf[..len],
                    Tag::from_slice(&tag),
                )
                .map_err(|_| invalid())?;
            if len == 0 {
                let mut tail = [0u8; 1];
                if input.read(&mut tail)? != 0 {
                    return Err(invalid());
                }
                return Ok(());
            }
            output.write_all(&buf[..len])?;
            index = index.checked_add(1).ok_or_else(invalid)?;
        }
    }

    pub fn seal_bytes(&self, bucket: &str, object: &str, bytes: &[u8]) -> io::Result<Vec<u8>> {
        let records = bytes
            .len()
            .div_ceil(CHUNK)
            .checked_add(1)
            .ok_or_else(invalid)?;
        let overhead = records
            .checked_mul(4 + TAG)
            .and_then(|n| n.checked_add(HEADER))
            .ok_or_else(invalid)?;
        let capacity = bytes.len().checked_add(overhead).ok_or_else(invalid)?;
        let mut out = Vec::with_capacity(capacity);
        self.seal(bucket, object, bytes, &mut out)?;
        Ok(out)
    }

    pub fn open_bytes(&self, bucket: &str, object: &str, bytes: &[u8]) -> io::Result<Vec<u8>> {
        if !Self::is_sealed(bytes) {
            return Err(invalid());
        }
        let mut out = Zeroizing::new(Vec::with_capacity(bytes.len()));
        self.open(bucket, object, bytes, &mut *out, bytes.len() as u64)?;
        Ok(std::mem::take(&mut *out))
    }
}

fn record_context(
    context: &[u8; 32],
    header: &[u8; HEADER],
    index: u64,
    len: u32,
) -> ([u8; 12], Vec<u8>) {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&index.to_be_bytes());
    let mut aad = Vec::with_capacity(84);
    aad.extend_from_slice(context);
    aad.extend_from_slice(header);
    aad.extend_from_slice(&index.to_be_bytes());
    aad.extend_from_slice(&len.to_be_bytes());
    (nonce, aad)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cipher(owner: &str) -> Cipher {
        Cipher::new([42; 32], "risc-box", owner, "deployment")
    }
    #[test]
    fn opens_independent_python_aes_gcm_vector() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/reference.json")).unwrap();
        let decode = |field: &str| -> Vec<u8> {
            v[field]
                .as_str()
                .unwrap()
                .as_bytes()
                .chunks_exact(2)
                .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
                .collect()
        };
        assert_eq!(
            cipher("alice")
                .open_bytes("bucket", "session", &decode("envelopeHex"))
                .unwrap(),
            decode("plaintextHex")
        );
    }
    #[test]
    fn roundtrip_empty_small_and_multichunk() {
        for size in [0, 19, CHUNK, CHUNK + 87, CHUNK * 2] {
            let plain = vec![123; size];
            let c = cipher("alice");
            let sealed = c.seal_bytes("bucket", "session", &plain).unwrap();
            assert_eq!(c.open_bytes("bucket", "session", &sealed).unwrap(), plain);
            assert_ne!(sealed, c.seal_bytes("bucket", "session", &plain).unwrap());
        }
    }
    #[test]
    fn owner_app_secret_bucket_and_object_are_bound() {
        let c = cipher("alice");
        let sealed = c.seal_bytes("bucket", "session", b"private RAM").unwrap();
        for bad in [
            cipher("bob"),
            Cipher::new([42; 32], "kvm-box-native", "alice", "deployment"),
            Cipher::new([41; 32], "risc-box", "alice", "deployment"),
        ] {
            assert!(bad.open_bytes("bucket", "session", &sealed).is_err());
        }
        assert!(c.open_bytes("other", "session", &sealed).is_err());
        assert!(c.open_bytes("bucket", "other", &sealed).is_err());
    }
    #[test]
    fn every_byte_and_every_truncation_is_authenticated() {
        let c = cipher("alice");
        let sealed = c.seal_bytes("b", "k", b"private RAM").unwrap();
        for n in 0..sealed.len() {
            let mut corrupt = sealed.clone();
            corrupt[n] ^= 1;
            assert!(c.open_bytes("b", "k", &corrupt).is_err(), "byte {n}");
            assert!(c.open_bytes("b", "k", &sealed[..n]).is_err(), "prefix {n}");
        }
        let mut extra = sealed.clone();
        extra.push(0);
        assert!(c.open_bytes("b", "k", &extra).is_err());
        assert!(c.open_bytes("b", "k", b"old plaintext snapshot").is_err());
    }
    #[test]
    fn rejects_reordering_splicing_and_output_over_limit() {
        let c = cipher("alice");
        let data = vec![5; CHUNK * 2];
        let sealed = c.seal_bytes("b", "k", &data).unwrap();
        let second = c.seal_bytes("b", "k", &data).unwrap();
        let end = HEADER + 4 + CHUNK + TAG;
        let mut splice = sealed.clone();
        splice[HEADER..end].copy_from_slice(&second[HEADER..end]);
        assert!(c.open_bytes("b", "k", &splice).is_err());
        let mut swapped = sealed.clone();
        swapped[HEADER..end].copy_from_slice(&sealed[end..end + 4 + CHUNK + TAG]);
        assert!(c.open_bytes("b", "k", &swapped).is_err());
        assert!(c
            .open("b", "k", &sealed[..], Vec::new(), data.len() as u64 - 1)
            .is_err());
    }
}
