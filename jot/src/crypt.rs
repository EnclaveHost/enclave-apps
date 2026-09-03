//! Encryption at rest, per owner: a note in the bucket is AES-256-GCM
//! ciphertext under a key only this attested app can derive.
//!
//! The deployment holds ONE master secret (`master_key` in the config,
//! referenced as a `$VAR` deployment secret, so it exists only in the guest
//! env). Every scope gets its own key from it:
//!
//!   K        = SHA-256(master_key)
//!   K_scope  = HMAC-SHA256(K, "jot-key-v1:" || scope)      scope = "shared" | "user:<sub>"
//!
//! and every object is
//!
//!   "JOT1" || nonce[12] || AES-256-GCM(K_scope, nonce, plaintext, aad = object key)
//!
//! The object key is authenticated data, so a ciphertext copied to another
//! name, or into another user's namespace, fails to open: the bucket cannot
//! be rearranged into a different notebook. A fresh random nonce per write
//! (getrandom) keeps the GCM contract; at one note per write the birthday
//! bound is not a concern here.
//!
//! What this does and does not protect, stated plainly: the bucket, its
//! operator, and anyone with the S3 credentials see ciphertext and names.
//! Whoever holds the master secret (the deployer, and the relay that stores
//! deployment secrets) could derive every key; per-user isolation is
//! therefore enforced by the verified identity in lib.rs, and encryption is
//! what keeps the bucket itself from being the weak point.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"JOT1";
const NONCE_LEN: usize = 12;

pub struct Cipher {
    key: [u8; 32],
}

impl Cipher {
    /// The cipher for one scope of one deployment.
    pub fn for_scope(master_key: &str, scope: &str) -> Cipher {
        let k = Sha256::digest(master_key.as_bytes());
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&k).expect("hmac key");
        mac.update(b"jot-key-v1:");
        mac.update(scope.as_bytes());
        let out = mac.finalize().into_bytes();
        let mut key = [0u8; 32];
        key.copy_from_slice(&out);
        Cipher { key }
    }

    /// Was this object written encrypted by this app?
    pub fn is_sealed(bytes: &[u8]) -> bool {
        bytes.len() >= MAGIC.len() + NONCE_LEN + 16 && &bytes[..4] == MAGIC
    }

    pub fn seal(&self, object_key: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| format!("no randomness for the nonce: {e}"))?;
        let aead = <Aes256Gcm as KeyInit>::new_from_slice(&self.key).map_err(|_| "bad key length")?;
        let ct = aead
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: object_key.as_bytes() })
            .map_err(|_| "encryption failed")?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, object_key: &str, sealed: &[u8]) -> Result<Vec<u8>, String> {
        if !Self::is_sealed(sealed) {
            return Err("object is not a sealed note".into());
        }
        let nonce = &sealed[MAGIC.len()..MAGIC.len() + NONCE_LEN];
        let ct = &sealed[MAGIC.len() + NONCE_LEN..];
        let aead = <Aes256Gcm as KeyInit>::new_from_slice(&self.key).map_err(|_| "bad key length")?;
        aead.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: object_key.as_bytes() })
            .map_err(|_| "cannot open this note: wrong master key, or the object was moved or altered".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_binding() {
        let c = Cipher::for_scope("master", "user:0xabc");
        let sealed = c.seal("notes/users/0xabc/a.md", b"hello").unwrap();
        assert!(Cipher::is_sealed(&sealed));
        assert!(!Cipher::is_sealed(b"hello"));
        assert_eq!(c.open("notes/users/0xabc/a.md", &sealed).unwrap(), b"hello");
        // another name, another user, another master: all refuse
        assert!(c.open("notes/users/0xabc/b.md", &sealed).is_err());
        assert!(Cipher::for_scope("master", "user:0xdef").open("notes/users/0xabc/a.md", &sealed).is_err());
        assert!(Cipher::for_scope("other", "user:0xabc").open("notes/users/0xabc/a.md", &sealed).is_err());
        // two seals of the same text differ (fresh nonces)
        assert_ne!(sealed, c.seal("notes/users/0xabc/a.md", b"hello").unwrap());
    }
}
