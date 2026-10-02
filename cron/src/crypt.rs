//! Encrypted durable scheduler state, adapted from Jot's AES-GCM format.
//! CRN1 || random nonce[12] || AES-256-GCM ciphertext, with the storage
//! object key as AAD. The HMAC-derived key uses the fixed scope "state";
//! tenant access checks happen in api.rs. The deployment secret holder can
//! decrypt the snapshot. Authentication detects alteration, not rollback.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"CRN1";
const NONCE_LEN: usize = 12;

pub struct Cipher {
    key: [u8; 32],
}

impl Cipher {
    /// The cipher for one scope of one deployment.
    pub fn for_scope(master_key: &str, scope: &str) -> Cipher {
        let k = Sha256::digest(master_key.as_bytes());
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&k).expect("hmac key");
        mac.update(b"enclave-cron-v1:");
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
        getrandom::getrandom(&mut nonce)
            .map_err(|e| format!("no randomness for the nonce: {e}"))?;
        let aead =
            <Aes256Gcm as KeyInit>::new_from_slice(&self.key).map_err(|_| "bad key length")?;
        let ct = aead
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: object_key.as_bytes(),
                },
            )
            .map_err(|_| "encryption failed")?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, object_key: &str, sealed: &[u8]) -> Result<Vec<u8>, String> {
        if !Self::is_sealed(sealed) {
            return Err("object is not sealed scheduler state".into());
        }
        let nonce = &sealed[MAGIC.len()..MAGIC.len() + NONCE_LEN];
        let ct = &sealed[MAGIC.len() + NONCE_LEN..];
        let aead =
            <Aes256Gcm as KeyInit>::new_from_slice(&self.key).map_err(|_| "bad key length")?;
        aead.decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: object_key.as_bytes(),
            },
        )
        .map_err(|_| {
            "cannot open this scheduler state: wrong master key, or the object was moved or altered"
                .into()
        })
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
        assert!(Cipher::for_scope("master", "user:0xdef")
            .open("notes/users/0xabc/a.md", &sealed)
            .is_err());
        assert!(Cipher::for_scope("other", "user:0xabc")
            .open("notes/users/0xabc/a.md", &sealed)
            .is_err());
        // two seals of the same text differ (fresh nonces)
        assert_ne!(sealed, c.seal("notes/users/0xabc/a.md", b"hello").unwrap());
    }
}
