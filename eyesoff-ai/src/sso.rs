//! Sign in with Enclave: verify the platform's SSO tokens inside the enclave.
//!
//! The flow (PLATFORM-sso.md is the full spec): the playground sends the
//! visitor to enclave.host's /sso/authorize, the platform authenticates them
//! however it already does (passkey or wallet), and redirects back with a
//! short-lived token in the URL fragment. Every gated request then carries it
//! as `Authorization: Bearer <token>`, and THIS module is what decides whether
//! the token is real - the check happens in here, on this deployment's own
//! CPU, so the platform's word is taken exactly once (at mint time, where it
//! is the only party that can vouch for a login) and never per request. No
//! outbound call, no session table, no state: signature + audience + expiry
//! is the whole decision, which is what a component that keeps nothing
//! between requests can actually enforce.
//!
//! Token format (EST1, "Enclave SSO Token v1"):
//!
//!   EST1.<base64url(claims JSON)>.<base64url(65-byte r||s||v signature)>
//!
//! The signature is an Ethereum EIP-191 personal_sign over the ASCII string
//! `EST1.<base64url(claims)>` - the exact bytes of the token up to the second
//! dot, so there is no canonicalization step to disagree about. secp256k1
//! with recovery, because the platform's whole identity model is Ethereum
//! keys: the deployment config names the signer as a plain ADDRESS a human
//! can eyeball against enclave.host's published one, and any wallet tool can
//! mint a test token (`cast wallet sign` writes exactly this signature).
//!
//! Claims: `{"v":1,"sub":<identity>,"aud":"0x<64-hex>","iat":<s>,"exp":<s>}`.
//! `sub` is the signed-in ACCOUNT: a relay account id (`acct_<hex>` - the
//! passkey is the person, no wallet required) or a bare wallet address.
//! `aud` is THIS deployment's id, and it is checked, not decorative: a
//! token minted for someone else's deployment must not open this one, however
//! valid its signature. Expiry is the only revocation there is (the app can
//! store no denylist), so the platform keeps TTLs short; skew_secs absorbs
//! clock drift on `iat` alone - a token from the near future is clock drift,
//! a token past `exp` is simply dead.

use serde::Deserialize;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use sha3::{Digest, Keccak256};

/// The `sso` block of the deployment config. Presence alone changes nothing
/// but /models advertising the flow; `required` is what closes the doors.
#[derive(Deserialize, Clone)]
pub struct SsoConfig {
    /// the platform SSO signing key's ADDRESS (0x + 40 hex). Not a secret -
    /// it is the pin against which every token is checked, the same address
    /// enclave.host publishes at /.well-known/sso-signer.json. Rotation is a
    /// config update here.
    pub signer: String,
    /// THIS deployment's id (0x + 64 hex), compared against every token's
    /// `aud`. Required, because without it any signed-in visitor to any
    /// deployment could replay their token here.
    pub audience: String,
    /// where the playground sends the visitor to sign in
    #[serde(default = "default_authorize_url")]
    pub authorize_url: String,
    /// true (the default): POST /chat and /title require a valid token, and
    /// /v1/* accepts one (beside api_key, if that is also set). false: nothing
    /// is gated - the playground merely offers the sign-in, for deployments
    /// that want the identity without the door.
    #[serde(default = "default_required")]
    pub required: bool,
    /// tolerated clock drift on `iat`, seconds
    #[serde(default = "default_skew")]
    pub skew_secs: u64,
}

fn default_authorize_url() -> String {
    "https://enclave.host/sso/authorize".into()
}
fn default_required() -> bool {
    true
}
fn default_skew() -> u64 {
    300
}

impl SsoConfig {
    /// The top-level `sso` block of the raw (merged) config, if one parses.
    /// A malformed block reads as None HERE, but it does not fail open: the
    /// same raw JSON feeds the AppConfig parse inside every gated handler,
    /// which then rejects the whole config loudly.
    pub fn from_raw(raw: &serde_json::Value) -> Option<SsoConfig> {
        raw.get("sso").cloned().and_then(|v| serde_json::from_value(v).ok())
    }
}

/// What a verified token says. Address subs normalize to lowercase 0x hex;
/// account-id subs pass through as minted.
#[derive(Debug)]
pub struct Claims {
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
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

    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| "malformed sign-in token payload")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|_| "malformed sign-in token payload")?;
    if claims.get("v").and_then(|v| v.as_u64()) != Some(1) {
        return Err("unsupported sign-in token version".into());
    }
    let sub = claims.get("sub").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let aud = claims.get("aud").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let iat = claims.get("iat").and_then(|v| v.as_u64()).unwrap_or(0);
    let exp = claims.get("exp").and_then(|v| v.as_u64()).unwrap_or(0);

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| "malformed sign-in token signature")?;
    if sig_bytes.len() != 65 {
        return Err("malformed sign-in token signature".into());
    }
    // wallets write v as 27/28, raw recovery ids are 0/1; accept both spellings
    let v = match sig_bytes[64] {
        27 | 28 => sig_bytes[64] - 27,
        b @ (0 | 1) => b,
        _ => return Err("malformed sign-in token signature".into()),
    };
    let sig = Signature::from_slice(&sig_bytes[..64])
        .map_err(|_| "malformed sign-in token signature")?;
    let recid = RecoveryId::try_from(v).map_err(|_| "malformed sign-in token signature")?;

    // the signed message is the token's own first two segments, byte-exact
    let prehash = eip191_hash(&format!("EST1.{payload_b64}"));
    let vk = VerifyingKey::recover_from_prehash(&prehash, &sig, recid)
        .map_err(|_| "sign-in token signature does not verify")?;
    if !hex_eq(&address_of(&vk), &cfg.signer) {
        return Err("sign-in token was not signed by this deployment's trusted signer".into());
    }
    if !hex_eq(&aud, &cfg.audience) {
        return Err("sign-in token was minted for a different deployment".into());
    }
    if iat > now_secs.saturating_add(cfg.skew_secs) {
        return Err("sign-in token is not valid yet".into());
    }
    if now_secs >= exp {
        return Err("sign-in expired; sign in again".into());
    }
    // the signed-in identity, in either of the platform's shapes: a relay
    // ACCOUNT id (acct_<hex> - the passkey IS the identity, no wallet
    // involved) or a bare wallet address (the original shape; addresses
    // canonicalize to lowercase, account ids pass through as minted)
    let addr_like = sub.len() == 42
        && sub.starts_with("0x")
        && sub[2..].chars().all(|c| c.is_ascii_hexdigit());
    let acct_like = (6..=80).contains(&sub.len())
        && sub.starts_with("acct_")
        && sub[5..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !(addr_like || acct_like) {
        return Err("malformed sign-in token payload".into());
    }
    let sub = if addr_like { sub.to_lowercase() } else { sub };
    Ok(Claims { sub, aud, iat, exp })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;

    // a fixed, obviously-throwaway key so every vector in this file (and the
    // one pasted into PLATFORM-sso.md) is reproducible
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[0x42u8; 32].into()).unwrap()
    }

    fn test_signer() -> String {
        address_of(test_key().verifying_key())
    }

    const AUD: &str = "0xcc1f4f3f000000000000000000000000000000000000000000000000000000aa";

    fn cfg() -> SsoConfig {
        SsoConfig {
            signer: test_signer(),
            audience: AUD.into(),
            authorize_url: default_authorize_url(),
            required: true,
            skew_secs: 300,
        }
    }

    /// Mint a token the way the platform will: claims -> b64url -> EIP-191
    /// personal_sign of "EST1.<b64>" -> append the 65-byte signature.
    fn mint(key: &SigningKey, claims: &serde_json::Value, v_style_27: bool) -> String {
        let payload_b64 = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        let prehash = eip191_hash(&format!("EST1.{payload_b64}"));
        let (sig, recid) = key.sign_prehash_recoverable(&prehash).unwrap();
        let mut bytes = sig.to_bytes().to_vec();
        bytes.push(recid.to_byte() + if v_style_27 { 27 } else { 0 });
        format!("EST1.{payload_b64}.{}", URL_SAFE_NO_PAD.encode(&bytes))
    }

    fn claims(sub: &str, aud: &str, iat: u64, exp: u64) -> serde_json::Value {
        serde_json::json!({ "v": 1, "sub": sub, "aud": aud, "iat": iat, "exp": exp })
    }

    const NOW: u64 = 1_800_000_000;
    const SUB: &str = "0x00a329c0648769a73afac7f9381e08fb43dbea72";

    #[test]
    fn roundtrip_both_v_spellings() {
        for v27 in [true, false] {
            let t = mint(&test_key(), &claims(SUB, AUD, NOW - 10, NOW + 3600), v27);
            let c = verify(&cfg(), &t, NOW).expect("valid token verifies");
            assert_eq!(c.sub, SUB);
            assert_eq!(c.exp, NOW + 3600);
        }
    }

    #[test]
    fn case_and_prefix_are_forgiven_in_pins() {
        let t = mint(&test_key(), &claims(SUB, &AUD.to_uppercase().replace("0X", "0x"), NOW, NOW + 60), true);
        let mut c = cfg();
        c.signer = c.signer.to_uppercase().replace("0X", "0x");
        assert!(verify(&c, &t, NOW).is_ok());
    }

    #[test]
    fn tampered_payload_fails() {
        let t = mint(&test_key(), &claims(SUB, AUD, NOW, NOW + 3600), true);
        // swap one payload character; the signature must stop matching
        let mut parts: Vec<String> = t.splitn(3, '.').map(String::from).collect();
        let flipped = if parts[1].ends_with('A') { "B" } else { "A" };
        let len = parts[1].len();
        parts[1].replace_range(len - 1..len, flipped);
        let t2 = parts.join(".");
        assert!(verify(&cfg(), &t2, NOW).is_err());
    }

    #[test]
    fn wrong_signer_fails() {
        let other = SigningKey::from_bytes(&[0x43u8; 32].into()).unwrap();
        let t = mint(&other, &claims(SUB, AUD, NOW, NOW + 3600), true);
        let e = verify(&cfg(), &t, NOW).unwrap_err();
        assert!(e.contains("trusted signer"), "{e}");
    }

    #[test]
    fn wrong_audience_fails() {
        let t = mint(&test_key(), &claims(SUB, &AUD.replace("aa", "bb"), NOW, NOW + 3600), true);
        let e = verify(&cfg(), &t, NOW).unwrap_err();
        assert!(e.contains("different deployment"), "{e}");
    }

    #[test]
    fn expiry_is_strict_and_iat_allows_skew() {
        let t = mint(&test_key(), &claims(SUB, AUD, NOW, NOW + 60), true);
        assert!(verify(&cfg(), &t, NOW + 59).is_ok());
        assert!(verify(&cfg(), &t, NOW + 60).is_err(), "exp second itself is dead");
        // 299s from the future is drift; 301s is not
        let t = mint(&test_key(), &claims(SUB, AUD, NOW + 299, NOW + 3600), true);
        assert!(verify(&cfg(), &t, NOW).is_ok());
        let t = mint(&test_key(), &claims(SUB, AUD, NOW + 301, NOW + 3600), true);
        assert!(verify(&cfg(), &t, NOW).is_err());
    }

    #[test]
    fn account_id_subs_are_identities_too() {
        let t = mint(&test_key(), &claims("acct_0e64d1897f10b32d3a1bc84e", AUD, NOW, NOW + 3600), true);
        let c = verify(&cfg(), &t, NOW).expect("account-id sub verifies");
        assert_eq!(c.sub, "acct_0e64d1897f10b32d3a1bc84e");
        // neither of the platform's shapes: refused
        for bad in ["bob", "acct_", "0x1234", ""] {
            let t = mint(&test_key(), &claims(bad, AUD, NOW, NOW + 3600), true);
            assert!(verify(&cfg(), &t, NOW).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn garbage_is_rejected_not_panicked() {
        for junk in ["", "EST1.", "EST1..", "EST1.!!.!!", "Bearer x", "EST2.a.b",
                     "EST1.eyJ2IjoxfQ", "EST1.eyJ2IjoxfQ.AAAA"] {
            assert!(verify(&cfg(), junk, NOW).is_err(), "{junk:?}");
        }
    }

    /// The vector quoted in PLATFORM-sso.md; if this moves, update the spec.
    #[test]
    fn spec_vector() {
        let t = mint(&test_key(), &claims(SUB, AUD, 1_755_000_000, 1_755_086_400), true);
        // regenerate the doc's literal: cargo test spec_vector -- --nocapture
        eprintln!("spec vector token: {t}");
        let c = verify(&cfg(), &t, 1_755_000_100).expect("spec vector verifies");
        assert_eq!(c.sub, SUB);
        // the fixed key's address, pinned so the doc and the code agree
        assert_eq!(test_signer(), "0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025");
    }
}
