//! Derived API keys: a personal, permanent credential grown out of a sign-in.
//!
//! The problem this solves: a sign-in token (sso.rs) is deliberately
//! short-lived - expiry is the only revocation a stateless verifier has - but
//! an API client wants a credential it can put in a config file and forget.
//! The deployment cannot mint-and-store random keys (a wasip2 component keeps
//! nothing between requests), so the key is DERIVED instead of issued: a MAC,
//! under a deployment secret, over the signed-in identity itself.
//!
//!   EAK1.<base64url(claims JSON)>.<base64url(32-byte MAC)>
//!
//! Claims are `{"sub":<identity>,"v":1}` - the same `sub` the platform put in
//! the sign-in token, which is what the user's wallet or passkey unlocked at
//! enclave.host. The credential itself can never reach this app (passkeys are
//! RP-bound to enclave.host, WalletConnect pairings are per dapp), so the
//! derivation chains through the one cryptographic fact the app can verify:
//! an EST1 token naming that identity. Wallet or passkey -> platform sign-in
//! -> EST1 sub -> this MAC. Same person, same key, every time.
//!
//! The MAC is Keccak-256 over `len(seed) || seed || "EAK1.<payload_b64>"`.
//! A plain prefix-keyed hash is sound HERE because Keccak is a sponge - there
//! is no length-extension attack to smuggle a longer message under the same
//! MAC, which is the one weakness HMAC exists to paper over in
//! Merkle-Damgard hashes - and the seed's length prefix pins the seed/message
//! boundary. The message is the key's own first two segments, byte-exact,
//! for the same reason EST1 signs its own segments: nothing to canonicalize,
//! nothing to disagree about.
//!
//! Properties, stated plainly because they are the contract:
//!   - DETERMINISTIC: deriving twice yields the identical key, so "show me my
//!     key again" is a fresh sign-in, not a database row. Nothing is stored,
//!     here or anywhere.
//!   - STATELESS to verify: recompute the MAC, compare in constant time.
//!   - NO EXPIRY: the trade a permanent credential makes. Revocation is
//!     rotating the seed (a secrets update), which revokes EVERY derived key
//!     at once - the honest capability of a keeper of no lists, and the doc
//!     says so rather than pretending otherwise.
//!
//! The seed lives in deployment SECRETS and is referenced from config as
//! `"api_key_seed": "$API_KEY_SEED"` - never the literal, config is published.
//! Anyone holding the seed can mint any identity's key, so it deserves the
//! same respect as a master api_key: 32+ random bytes.

use crate::sso::canonical_sub;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha3::{Digest, Keccak256};

/// The MAC under the deployment seed: Keccak-256 of the length-pinned seed
/// followed by the exact bytes being authenticated.
fn mac(seed: &str, msg: &str) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update((seed.len() as u64).to_le_bytes());
    h.update(seed.as_bytes());
    h.update(msg.as_bytes());
    h.finalize().into()
}

/// Byte comparison that spends the same time agreeing and disagreeing, so a
/// guessing client learns nothing from response timing.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Derive the one key this identity has under this seed. `sub` must already
/// be a verified sign-in's subject; this function trusts its caller on that,
/// because the caller (the /v1/keys handler) is the one holding the verified
/// EST1 claims.
pub fn derive(seed: &str, sub: &str) -> Option<String> {
    let sub = canonical_sub(sub)?;
    // serde_json's default map is ordered (BTreeMap), so this serialization
    // is deterministic - and even if it ever changed, old keys stay valid:
    // verification MACs the received bytes, not a re-serialization
    let payload = serde_json::json!({ "sub": sub, "v": 1 }).to_string();
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let tag = mac(seed, &format!("EAK1.{payload_b64}"));
    Some(format!("EAK1.{payload_b64}.{}", URL_SAFE_NO_PAD.encode(tag)))
}

/// Verify a presented key against the seed; the verified identity comes back.
/// Errors are user-facing prose for the 401 body, and none of them separate
/// "no such identity" from "bad MAC" - a guessing client sees one refusal.
pub fn verify(seed: &str, key: &str) -> Result<String, String> {
    const BAD: &str = "invalid API key";
    let rest = key.strip_prefix("EAK1.").ok_or(BAD)?;
    let (payload_b64, tag_b64) = rest.split_once('.').ok_or(BAD)?;
    let tag = URL_SAFE_NO_PAD.decode(tag_b64).map_err(|_| BAD)?;
    let expect = mac(seed, &format!("EAK1.{payload_b64}"));
    if !ct_eq(&tag, &expect) {
        return Err(BAD.into());
    }
    // MAC first, shape second: only bytes this deployment itself once encoded
    // get parsed at all
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).map_err(|_| BAD)?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).map_err(|_| BAD)?;
    if claims.get("v").and_then(|v| v.as_u64()) != Some(1) {
        return Err(BAD.into());
    }
    let sub = claims.get("sub").and_then(|v| v.as_str()).ok_or(BAD)?;
    canonical_sub(sub).ok_or_else(|| BAD.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

    #[test]
    fn derivation_is_deterministic_and_verifies() {
        for sub in ["acct_0e64d1897f10b32d3a1bc84e", "0x00A329c0648769A73afAc7F9381E08FB43dBEA72"] {
            let k1 = derive(SEED, sub).expect("derives");
            let k2 = derive(SEED, sub).expect("derives");
            assert_eq!(k1, k2, "same identity, same key");
            let got = verify(SEED, &k1).expect("verifies");
            assert_eq!(got, canonical_sub(sub).unwrap());
        }
    }

    #[test]
    fn addresses_derive_canonically() {
        // the checksummed and lowercase spellings of one address are one key
        let a = derive(SEED, "0x00A329c0648769A73afAc7F9381E08FB43dBEA72").unwrap();
        let b = derive(SEED, "0x00a329c0648769a73afac7f9381e08fb43dbea72").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_identities_and_seeds_differ() {
        let a = derive(SEED, "acct_0e64d1897f10b32d3a1bc84e").unwrap();
        let b = derive(SEED, "acct_0e64d1897f10b32d3a1bc84f").unwrap();
        assert_ne!(a, b);
        let c = derive("another-seed", "acct_0e64d1897f10b32d3a1bc84e").unwrap();
        assert_ne!(a, c);
        assert!(verify("another-seed", &a).is_err(), "wrong seed refuses");
    }

    #[test]
    fn tampering_fails() {
        let k = derive(SEED, "acct_0e64d1897f10b32d3a1bc84e").unwrap();
        let mut parts: Vec<String> = k.splitn(3, '.').map(String::from).collect();
        // forge a different sub under the old MAC
        parts[1] = URL_SAFE_NO_PAD.encode(br#"{"sub":"acct_ffffffffffffffffffffffff","v":1}"#);
        assert!(verify(SEED, &parts.join(".")).is_err());
        // flip one MAC character
        let mut parts: Vec<String> = k.splitn(3, '.').map(String::from).collect();
        let len = parts[2].len();
        let flipped = if parts[2].ends_with('A') { "B" } else { "A" };
        parts[2].replace_range(len - 1..len, flipped);
        assert!(verify(SEED, &parts.join(".")).is_err());
    }

    #[test]
    fn only_platform_identity_shapes_derive() {
        for bad in ["bob", "acct_", "0x1234", "", "acct_has spaces"] {
            assert!(derive(SEED, bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn garbage_is_rejected_not_panicked() {
        for junk in ["", "EAK1.", "EAK1..", "EAK1.!!.!!", "EST1.a.b", "sk-proj-abc123",
                     "EAK1.eyJ2IjoxfQ", "EAK1.eyJ2IjoxfQ.AAAA"] {
            assert!(verify(SEED, junk).is_err(), "{junk:?}");
        }
    }

    /// Pinned vector: if this moves, previously handed-out keys stop
    /// verifying, which is a breaking change to call out loudly - not an
    /// implementation detail to shrug at.
    #[test]
    fn pinned_vector() {
        let k = derive(SEED, "acct_0e64d1897f10b32d3a1bc84e").unwrap();
        assert_eq!(
            k,
            "EAK1.eyJzdWIiOiJhY2N0XzBlNjRkMTg5N2YxMGIzMmQzYTFiYzg0ZSIsInYiOjF9.\
             uZIhoolqA1yzEDBoI6RM-ckj5RjieCQvWmtPxbB1InI"
        );
    }
}
