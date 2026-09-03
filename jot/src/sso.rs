//! Sign in with Enclave, verified here: the platform's EST1 tokens name the
//! signed-in account, and that name is what a note belongs to.
//!
//! A port of eyesoff-ai's sso.rs (PLATFORM-sso.md there is the spec), kept
//! byte-compatible so one token means the same identity in both apps:
//!
//!   EST1.<base64url(claims JSON)>.<base64url(65-byte r||s||v signature)>
//!
//! The signature is an EIP-191 personal_sign over the ASCII string
//! `EST1.<base64url(claims)>`, the token's own first two segments, by the
//! platform's SSO key. Claims: `{"v":1,"sub":<identity>,"aud":"0x<64-hex>",
//! "iat":<s>,"exp":<s>}`. `sub` is a relay account id (`acct_<hex>`) or a
//! lowercase wallet address.
//!
//! What differs from eyesoff-ai: this app accepts MORE THAN ONE audience.
//! Its own deployment id (the UI's sign-in) and the ids of the deployments
//! it serves as a notebook (an eyesoff-ai instance forwarding the token it
//! verified). A token minted for any other deployment is refused, however
//! valid its signature.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use sha3::{Digest, Keccak256};

/// The `sso` block of the app config. Its presence turns on per-user mode.
#[derive(Clone)]
pub struct SsoConfig {
    /// the platform SSO signing key's ADDRESS (0x + 40 hex), the same one
    /// enclave.host publishes at /.well-known/sso-signer.json
    pub signer: String,
    /// THIS deployment's id (0x + 64 hex): what the UI signs in against
    pub audience: String,
    /// other deployment ids whose tokens are accepted: the eyesoff-ai
    /// instances this notebook serves, which forward the tokens they verified
    pub accept: Vec<String>,
    /// where the UI sends a visitor to sign in
    pub authorize_url: String,
    /// tolerated clock drift on `iat`, seconds
    pub skew_secs: u64,
}

impl SsoConfig {
    pub fn from_config(v: &serde_json::Value) -> Result<Option<SsoConfig>, String> {
        let Some(s) = v.get("sso") else { return Ok(None) };
        let str_of = |k: &str| s.get(k).and_then(|x| x.as_str()).map(str::trim).unwrap_or("").to_string();
        let signer = str_of("signer");
        let audience = str_of("audience");
        if !is_hex(&signer, 40) {
            return Err("sso.signer must be the platform SSO signer address (0x + 40 hex)".into());
        }
        if !is_hex(&audience, 64) {
            return Err("sso.audience must be this deployment's id (0x + 64 hex)".into());
        }
        let mut accept = Vec::new();
        if let Some(a) = s.get("accept") {
            let Some(arr) = a.as_array() else { return Err("sso.accept must be an array of deployment ids".into()) };
            for x in arr {
                let id = x.as_str().map(str::trim).unwrap_or("").to_string();
                if !is_hex(&id, 64) {
                    return Err("sso.accept entries must be deployment ids (0x + 64 hex)".into());
                }
                accept.push(id);
            }
        }
        let authorize_url = {
            let u = str_of("authorize_url");
            if u.is_empty() { "https://enclave.host/sso/authorize".to_string() } else { u }
        };
        let skew_secs = s.get("skew_secs").and_then(|x| x.as_u64()).unwrap_or(300);
        Ok(Some(SsoConfig { signer, audience, accept, authorize_url, skew_secs }))
    }
}

fn is_hex(s: &str, digits: usize) -> bool {
    s.len() == 2 + digits && s.starts_with("0x") && s[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// What a verified token says that this app acts on: the account. The
/// audience and expiry are checked in `verify` and need no reader after it.
#[derive(Debug)]
pub struct Claims {
    pub sub: String,
}

fn keccak(bytes: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(bytes);
    h.finalize().into()
}

/// EIP-191 "personal message" hash of `msg`: what wallets and the platform
/// signer actually sign.
fn eip191_hash(msg: &str) -> [u8; 32] {
    keccak(format!("\u{19}Ethereum Signed Message:\n{}{}", msg.len(), msg).as_bytes())
}

/// 0x-hex address of a recovered public key, lowercase.
fn address_of(vk: &VerifyingKey) -> String {
    let point = vk.to_encoded_point(false);
    let digest = keccak(&point.as_bytes()[1..]);
    let mut s = String::with_capacity(42);
    s.push_str("0x");
    for b in &digest[12..] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Is `sub` one of the platform's two identity shapes? A relay ACCOUNT id
/// (acct_<hex>) or a bare wallet address. Returns the canonical form:
/// addresses lowercase, account ids as minted. Both are safe as a single
/// object-key segment, which is what the namespace needs of them.
pub fn canonical_sub(sub: &str) -> Option<String> {
    let addr_like = sub.len() == 42
        && sub.starts_with("0x")
        && sub[2..].chars().all(|c| c.is_ascii_hexdigit());
    let acct_like = (6..=80).contains(&sub.len())
        && sub.starts_with("acct_")
        && sub[5..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if addr_like {
        Some(sub.to_lowercase())
    } else if acct_like {
        Some(sub.to_string())
    } else {
        None
    }
}

/// hex comparison that forgives 0x and case, nothing else
fn hex_eq(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.trim().trim_start_matches("0x").trim_start_matches("0X").to_lowercase();
    let (a, b) = (strip(a), strip(b));
    !a.is_empty() && a == b
}

/// Verify one token against this deployment's pins. Errors are user-facing
/// prose (they land in the 401 body); none of them leak what WAS expected.
pub fn verify(cfg: &SsoConfig, token: &str, now_secs: u64) -> Result<Claims, String> {
    let rest = token.strip_prefix("EST1.").ok_or("not an Enclave sign-in token")?;
    let (payload_b64, sig_b64) = rest.split_once('.').ok_or("malformed sign-in token")?;

    let payload = URL_SAFE_NO_PAD.decode(payload_b64).map_err(|_| "malformed sign-in token payload")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|_| "malformed sign-in token payload")?;
    if claims.get("v").and_then(|v| v.as_u64()) != Some(1) {
        return Err("unsupported sign-in token version".into());
    }
    let sub = claims.get("sub").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let aud = claims.get("aud").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let iat = claims.get("iat").and_then(|v| v.as_u64()).unwrap_or(0);
    let exp = claims.get("exp").and_then(|v| v.as_u64()).unwrap_or(0);

    let sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| "malformed sign-in token signature")?;
    if sig_bytes.len() != 65 {
        return Err("malformed sign-in token signature".into());
    }
    // wallets write v as 27/28, raw recovery ids are 0/1; accept both spellings
    let v = match sig_bytes[64] {
        27 | 28 => sig_bytes[64] - 27,
        b @ (0 | 1) => b,
        _ => return Err("malformed sign-in token signature".into()),
    };
    let sig = Signature::from_slice(&sig_bytes[..64]).map_err(|_| "malformed sign-in token signature")?;
    let recid = RecoveryId::try_from(v).map_err(|_| "malformed sign-in token signature")?;

    // the signed message is the token's own first two segments, byte-exact
    let prehash = eip191_hash(&format!("EST1.{payload_b64}"));
    let vk = VerifyingKey::recover_from_prehash(&prehash, &sig, recid)
        .map_err(|_| "sign-in token signature does not verify")?;
    if !hex_eq(&address_of(&vk), &cfg.signer) {
        return Err("sign-in token was not signed by this deployment's trusted signer".into());
    }
    if !hex_eq(&aud, &cfg.audience) && !cfg.accept.iter().any(|a| hex_eq(&aud, a)) {
        return Err("sign-in token was minted for a deployment this notebook does not serve".into());
    }
    if iat > now_secs.saturating_add(cfg.skew_secs) {
        return Err("sign-in token is not valid yet".into());
    }
    if now_secs >= exp {
        return Err("sign-in expired; sign in again".into());
    }
    let sub = canonical_sub(&sub).ok_or("malformed sign-in token payload")?;
    Ok(Claims { sub })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUD: &str = "0xcc1f4f3f000000000000000000000000000000000000000000000000000000aa";
    const SUB: &str = "0x00a329c0648769a73afac7f9381e08fb43dbea72";
    /// PLATFORM-sso.md's vector: key 0x42..42, whose address is below
    const SPEC: &str = "EST1.eyJhdWQiOiIweGNjMWY0ZjNmMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwYWEiLCJleHAiOjE3NTUwODY0MDAsImlhdCI6MTc1NTAwMDAwMCwic3ViIjoiMHgwMGEzMjljMDY0ODc2OWE3M2FmYWM3ZjkzODFlMDhmYjQzZGJlYTcyIiwidiI6MX0.yk7Y_U0V-3ZyKhLJptbXZB3_Id-bEay1FtUTLFWjGgdyYQRL3xUJfKR5WTawlUSttKpUO0_H-x960Vn5-82NvRw";

    fn cfg(audience: &str, accept: &[&str]) -> SsoConfig {
        SsoConfig {
            signer: "0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025".into(),
            audience: audience.into(),
            accept: accept.iter().map(|s| s.to_string()).collect(),
            authorize_url: "https://enclave.host/sso/authorize".into(),
            skew_secs: 300,
        }
    }

    #[test]
    fn spec_vector_verifies_for_own_and_accepted_audiences() {
        let now = 1_755_000_100;
        let c = verify(&cfg(AUD, &[]), SPEC, now).expect("own audience");
        assert_eq!(c.sub, SUB);
        let other = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let c = verify(&cfg(other, &[AUD]), SPEC, now).expect("accepted audience");
        assert_eq!(c.sub, SUB);
        let e = verify(&cfg(other, &[]), SPEC, now).unwrap_err();
        assert!(e.contains("does not serve"), "{e}");
    }

    #[test]
    fn expiry_signer_and_garbage() {
        assert!(verify(&cfg(AUD, &[]), SPEC, 1_755_086_400).is_err(), "exp second is dead");
        let mut c = cfg(AUD, &[]);
        c.signer = "0x0000000000000000000000000000000000000001".into();
        assert!(verify(&c, SPEC, 1_755_000_100).unwrap_err().contains("trusted signer"));
        for junk in ["", "EST1.", "EST1..", "EST1.!!.!!", "Bearer x", "EST2.a.b"] {
            assert!(verify(&cfg(AUD, &[]), junk, 1_755_000_100).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn config_block_parses_and_validates() {
        let v: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"sso":{{"signer":"0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025","audience":"{AUD}","accept":["{AUD}"]}}}}"#
        )).unwrap();
        let c = SsoConfig::from_config(&v).unwrap().unwrap();
        assert_eq!(c.accept.len(), 1);
        assert_eq!(c.skew_secs, 300);
        assert!(SsoConfig::from_config(&serde_json::json!({})).unwrap().is_none());
        assert!(SsoConfig::from_config(&serde_json::json!({"sso": {"signer": "nope", "audience": AUD}})).is_err());
        assert!(SsoConfig::from_config(&serde_json::json!({"sso": {"signer": "0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025", "audience": "0x12"}})).is_err());
    }

    #[test]
    fn subs_are_key_safe() {
        assert_eq!(canonical_sub("0x00A329c0648769A73afAc7F9381E08FB43dBEA72").as_deref(), Some(SUB));
        assert_eq!(canonical_sub("acct_0e64d1897f10b32d3a1bc84e").as_deref(), Some("acct_0e64d1897f10b32d3a1bc84e"));
        for bad in ["bob", "acct_", "0x1234", "", "acct_a/b", "acct_a b"] {
            assert!(canonical_sub(bad).is_none(), "{bad:?}");
        }
    }
}
