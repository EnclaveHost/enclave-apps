//! Shared configuration policy. Private saves always require an authenticated
//! owner and a secret. Plaintext is accepted only for explicitly pinned public
//! templates, never as a fallback after failed authentication.
use crate::Cipher;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

pub struct Policy {
    cipher: Option<Cipher>,
    public: BTreeMap<String, [u8; 32]>,
}

fn hex32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.is_ascii() {
        return Err("expected 64 hexadecimal characters".into());
    }
    let mut out = [0u8; 32];
    for (i, part) in value.as_bytes().chunks_exact(2).enumerate() {
        let a = (part[0] as char)
            .to_digit(16)
            .ok_or("expected hexadecimal characters")?;
        let b = (part[1] as char)
            .to_digit(16)
            .ok_or("expected hexadecimal characters")?;
        out[i] = ((a << 4) | b) as u8;
    }
    Ok(out)
}

impl Policy {
    pub fn from_config(config: &Value, app: &str, authenticated: bool) -> Result<Self, String> {
        let mut public = BTreeMap::new();
        if let Some(v) = config.get("publicSnapshots") {
            for (key, value) in v
                .as_object()
                .ok_or("publicSnapshots must map object keys to SHA-256 hashes")?
            {
                if key.is_empty() {
                    return Err("empty public snapshot key".into());
                }
                public.insert(
                    key.clone(),
                    hex32(
                        value
                            .as_str()
                            .ok_or("public snapshot hash must be a string")?,
                    )?,
                );
            }
        }
        let cipher = match config.get("snapshotEncryption") {
            None => None,
            Some(v) => {
                if !authenticated {
                    return Err("private snapshots require owner authentication (api_key)".into());
                }
                let obj = v
                    .as_object()
                    .ok_or("snapshotEncryption must be an object")?;
                if obj
                    .keys()
                    .any(|k| !matches!(k.as_str(), "masterKey" | "owner" | "deployment"))
                {
                    return Err("unknown snapshotEncryption field".into());
                }
                let field = |name: &str| -> Result<&str, String> {
                    obj.get(name)
                        .and_then(Value::as_str)
                        .filter(|s| {
                            !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                        })
                        .ok_or_else(|| format!("snapshotEncryption.{name} is required"))
                };
                let master = Zeroizing::new(hex32(field("masterKey")?).map_err(|_| "snapshotEncryption.masterKey must be a random 32-byte secret encoded as 64 hex characters")?);
                if master.iter().all(|b| *b == 0) {
                    return Err("snapshot master key must not be all zero".into());
                }
                Some(Cipher::new(
                    *master,
                    app,
                    field("owner")?,
                    field("deployment")?,
                ))
            }
        };
        Ok(Self { cipher, public })
    }

    pub fn is_public(&self, key: &str) -> bool {
        self.public.contains_key(key)
    }

    pub fn check_save(&self, key: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("private snapshot object key is required".into());
        }
        if self.is_public(key) {
            return Err(
                "public snapshot templates are read-only; choose a private snapshot key".into(),
            );
        }
        if self.cipher.is_none() {
            return Err(
                "private snapshot saving requires snapshotEncryption and owner authentication"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn seal(&self, bucket: &str, key: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.check_save(key)?;
        self.cipher
            .as_ref()
            .expect("checked above")
            .seal_bytes(bucket, key, bytes)
            .map_err(|e| e.to_string())
    }

    /// Offline migration only: returns the configured cipher after enforcing
    /// private-object policy. The caller must stage decrypted output privately.
    pub fn private_cipher(&self, key: &str) -> Result<&Cipher, String> {
        self.check_save(key)?;
        Ok(self.cipher.as_ref().expect("checked above"))
    }

    pub fn open(&self, bucket: &str, key: &str, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        if let Some(hash) = self.public.get(key) {
            if Cipher::is_sealed(&bytes) || Sha256::digest(&bytes).as_slice() != hash {
                return Err(
                    "public snapshot hash mismatch or encrypted object at public key".into(),
                );
            }
            return Ok(bytes);
        }
        let c = self.cipher.as_ref().ok_or("private snapshot loading requires snapshotEncryption; public templates need a pinned publicSnapshots hash")?;
        c.open_bytes(bucket, key, &bytes).map_err(|_| "private snapshot authentication failed; refusing plaintext, wrong owner/key, or damaged snapshot".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Value {
        serde_json::json!({"snapshotEncryption":{"masterKey":"ab".repeat(32),"owner":"alice","deployment":"deploy-1"}})
    }
    #[test]
    fn no_secret_no_plaintext_save_or_load() {
        let p = Policy::from_config(&Value::Null, "risc-box", false).unwrap();
        assert!(p.seal("b", "k", b"RAM").is_err());
        assert!(p.open("b", "k", b"RAM".to_vec()).is_err());
    }
    #[test]
    fn authentication_and_valid_explicit_configuration_required() {
        assert!(Policy::from_config(&config(), "risc-box", false).is_err());
        for field in ["masterKey", "owner", "deployment"] {
            let mut v = config();
            v["snapshotEncryption"][field] = Value::Null;
            assert!(Policy::from_config(&v, "risc-box", true).is_err());
        }
        for key in ["", "$UNRESOLVED", "00", &"00".repeat(32), &"é".repeat(32)] {
            let mut v = config();
            v["snapshotEncryption"]["masterKey"] = key.into();
            assert!(Policy::from_config(&v, "risc-box", true).is_err());
        }
        let mut v = config();
        v["snapshotEncryption"]["enabled"] = false.into();
        assert!(Policy::from_config(&v, "risc-box", true).is_err());
    }
    #[test]
    fn pinned_public_template_cannot_be_overwritten_or_substituted() {
        let plain = b"public boot template";
        let hash: String = Sha256::digest(plain)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut v = config();
        v["publicSnapshots"] = serde_json::json!({"public": hash});
        let p = Policy::from_config(&v, "risc-box", true).unwrap();
        assert_eq!(p.open("b", "public", plain.to_vec()).unwrap(), plain);
        assert!(p.open("b", "public", b"private RAM".to_vec()).is_err());
        assert!(p.seal("b", "public", b"private RAM").is_err());
        let private = p.seal("b", "private", b"private RAM").unwrap();
        assert_eq!(p.open("b", "private", private).unwrap(), b"private RAM");
        assert!(p.open("b", "private", plain.to_vec()).is_err());
    }
    #[test]
    fn deployment_is_bound_and_provider_is_not() {
        let a = Policy::from_config(&config(), "risc-box", true).unwrap();
        let bytes = a.seal("b", "k", b"state").unwrap();
        let mut other = config();
        other["snapshotEncryption"]["deployment"] = "deploy-2".into();
        let b = Policy::from_config(&other, "risc-box", true).unwrap();
        assert!(b.open("b", "k", bytes.clone()).is_err());
        // A different S3 endpoint can hold the identical opaque object.
        other = config();
        other["endpoint"] = "https://generic-s3.example".into();
        let migrated = Policy::from_config(&other, "risc-box", true).unwrap();
        assert_eq!(migrated.open("b", "k", bytes).unwrap(), b"state");
    }
}
