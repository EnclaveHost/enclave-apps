//! Relay-independent, deployment-bound EIP-191 upload authorization.
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use sha3::{Digest, Keccak256};

pub fn canonical_hex(s: &str, bytes: usize, prefix: bool) -> bool {
    let raw = if prefix { s.strip_prefix("0x").unwrap_or("") } else { s };
    raw.len() == bytes * 2 && raw.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub fn message(service: &str, path: &str, address: &str, hash: &str, bytes: u64, expiry: u64) -> String {
    format!("enclave-storage-upload:v1\nchainId:8453\nservice:{service}\nmethod:POST\npath:{path}\naddress:{address}\nsha256:{hash}\nbytes:{bytes}\nexpiry:{expiry}")
}

fn digest(message: &str) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(format!("\x19Ethereum Signed Message:\n{}", message.len()));
    h.update(message.as_bytes());
    h.finalize().into()
}

pub fn verify(address: &str, message: &str, signature: &str) -> bool {
    if !canonical_hex(address, 20, true) { return false; }
    let Some(raw) = signature.strip_prefix("0x") else { return false; };
    if raw.len() != 130 || !raw.bytes().all(|c| c.is_ascii_hexdigit()) { return false; }
    let bytes: Vec<u8> = (0..65).map(|i| u8::from_str_radix(&raw[i*2..i*2+2], 16).unwrap()).collect();
    let Ok(sig) = Signature::from_slice(&bytes[..64]) else { return false; };
    let v = match bytes[64] { 0 | 1 => bytes[64], 27 | 28 => bytes[64] - 27, _ => return false };
    let Some(recovery) = RecoveryId::from_byte(v) else { return false; };
    let Ok(key) = VerifyingKey::recover_from_prehash(&digest(message), &sig, recovery) else { return false; };
    let hash = Keccak256::digest(&key.to_encoded_point(false).as_bytes()[1..]);
    let recovered = format!("0x{}", hash[12..].iter().map(|b| format!("{b:02x}")).collect::<String>());
    recovered == address
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;
    #[test]
    fn viem_personal_sign_vector() {
        let v: serde_json::Value = serde_json::from_str(include_str!("wallet-upload-vector.json")).unwrap();
        let m = message(v["service"].as_str().unwrap(), v["path"].as_str().unwrap(), v["address"].as_str().unwrap(), v["sha256"].as_str().unwrap(), v["bytes"].as_u64().unwrap(), v["expiry"].as_u64().unwrap());
        assert_eq!(m, v["message"].as_str().unwrap());
        assert!(verify(v["address"].as_str().unwrap(), &m, v["signature"].as_str().unwrap()));
    }
    #[test]
    fn signed_scope_cannot_be_changed() {
        let key = SigningKey::from_bytes(&[0x42u8;32].into()).unwrap();
        let h = Keccak256::digest(&key.verifying_key().to_encoded_point(false).as_bytes()[1..]);
        let address = format!("0x{}", h[12..].iter().map(|b| format!("{b:02x}")).collect::<String>());
        let service = format!("0x{}", "ab".repeat(32));
        let hash = "cd".repeat(32);
        let original = message(&service, "/add-json", &address, &hash, 2, 1000);
        let (sig, rec) = key.sign_prehash_recoverable(&digest(&original)).unwrap();
        let signature = format!("0x{}{:02x}", sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(), rec.to_byte()+27);
        assert!(verify(&address, &original, &signature));
        for altered in [message(&format!("0x{}", "ef".repeat(32)), "/add-json", &address, &hash, 2, 1000), message(&service,"/add-wasm",&address,&hash,2,1000), message(&service,"/add-json",&address,&"00".repeat(32),2,1000), message(&service,"/add-json",&address,&hash,3,1000), message(&service,"/add-json",&address,&hash,2,1001)] {
            assert!(!verify(&address, &altered, &signature));
        }
        assert!(!verify(&format!("0x{}", "00".repeat(20)), &original, &signature));
        assert!(!verify(&address, &original, "0x00"));
        assert!(!verify(&address, &original, &format!("{}ff", &signature[..130])));
    }
}
