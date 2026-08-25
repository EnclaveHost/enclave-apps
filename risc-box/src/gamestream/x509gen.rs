//! A minimal self-signed X.509 v3 generator, in DER, by hand.
//!
//! The GameStream host needs a stable RSA identity: Moonlight pins the server
//! certificate at pairing time and checks it on every later connection, and the
//! HTTPS control surface serves under it.
//!
//! Hand-rolled because every off-the-shelf generator is unavailable here.
//! `rcgen` is the obvious choice and each of its crypto backends (`ring`,
//! `aws-lc-rs`) is C that will not cross-compile to wasm32-wasip2 — the same
//! wall openssl hit, and the reason this port exists at all. What is left is
//! pure-Rust RSA plus about a page of TLV, which is a fair trade for a
//! certificate whose shape never varies.
//!
//! Scope is deliberately one certificate: v3, self-signed, RSA-2048,
//! sha256WithRSAEncryption, one CN. Not a general X.509 library.

use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::Sha256;

// --- DER primitives -------------------------------------------------------

const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_NULL: u8 = 0x05;
const TAG_OID: u8 = 0x06;
const TAG_UTF8: u8 = 0x0c;
const TAG_UTCTIME: u8 = 0x17;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;

/// DER length: short form under 128, else big-endian minimal long form.
fn len_bytes(n: usize) -> Vec<u8> {
    if n < 0x80 {
        return vec![n as u8];
    }
    let mut be = n.to_be_bytes().to_vec();
    while be.first() == Some(&0) {
        be.remove(0);
    }
    let mut out = vec![0x80 | be.len() as u8];
    out.extend_from_slice(&be);
    out
}

fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&len_bytes(value.len()));
    out.extend_from_slice(value);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, &parts.concat())
}

/// DER INTEGER: two's complement, minimal, so a leading high bit needs a 0x00
/// pad or the value reads as negative.
fn integer(bytes: &[u8]) -> Vec<u8> {
    let mut v: &[u8] = bytes;
    while v.len() > 1 && v[0] == 0 && v[1] & 0x80 == 0 {
        v = &v[1..];
    }
    let mut body = Vec::with_capacity(v.len() + 1);
    if v.first().map_or(true, |b| b & 0x80 != 0) {
        body.push(0);
    }
    body.extend_from_slice(v);
    tlv(TAG_INTEGER, &body)
}

/// BIT STRING with no unused trailing bits — the only form used here.
fn bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(0); // unused bits
    body.extend_from_slice(bytes);
    tlv(TAG_BIT_STRING, &body)
}

/// Encode a dotted OID. First two arcs pack into one byte as 40*a + b.
fn oid(arcs: &[u32]) -> Vec<u8> {
    let mut body = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut stack = Vec::new();
        let mut v = arc;
        stack.push((v & 0x7f) as u8);
        v >>= 7;
        while v > 0 {
            stack.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        stack.reverse();
        body.extend_from_slice(&stack);
    }
    tlv(TAG_OID, &body)
}

/// `[n]` explicit context tag wrapping one value.
fn explicit(n: u8, value: &[u8]) -> Vec<u8> {
    tlv(0xa0 | n, value)
}

fn oid_sha256_with_rsa() -> Vec<u8> {
    oid(&[1, 2, 840, 113549, 1, 1, 11])
}

fn oid_rsa_encryption() -> Vec<u8> {
    oid(&[1, 2, 840, 113549, 1, 1, 1])
}

fn oid_common_name() -> Vec<u8> {
    oid(&[2, 5, 4, 3])
}

/// AlgorithmIdentifier with explicit NULL parameters, which RSA requires.
fn alg_sha256_rsa() -> Vec<u8> {
    seq(&[oid_sha256_with_rsa(), tlv(TAG_NULL, &[])])
}

/// UTCTime, `YYMMDDHHMMSSZ`. Valid to 2049, which outlives any certificate
/// this generates (10 year life).
fn utc_time(unix: i64) -> Vec<u8> {
    let (y, mo, d, h, mi, s) = civil_from_unix(unix);
    let text = format!(
        "{:02}{:02}{:02}{:02}{:02}{:02}Z",
        y % 100,
        mo,
        d,
        h,
        mi,
        s
    );
    tlv(TAG_UTCTIME, text.as_bytes())
}

/// days-from-civil, inverted (Howard Hinnant's algorithm). Avoids pulling a
/// date crate in for two timestamps.
fn civil_from_unix(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

// --- the certificate ------------------------------------------------------

/// Build a self-signed v3 certificate for `common_name`, valid from `now_unix`
/// for `days`, signed by `key` with SHA-256.
pub fn self_signed(
    key: &RsaPrivateKey,
    common_name: &str,
    now_unix: i64,
    days: i64,
    serial: &[u8],
) -> Result<Vec<u8>, String> {
    let pub_der = RsaPublicKey::from(key)
        .to_pkcs1_der()
        .map_err(|e| format!("rsa public key: {e}"))?;
    let spki = seq(&[
        seq(&[oid_rsa_encryption(), tlv(TAG_NULL, &[])]),
        bit_string(pub_der.as_bytes()),
    ]);

    // Name ::= SEQUENCE OF RelativeDistinguishedName (SET OF AttributeTypeAndValue)
    let name = seq(&[tlv(
        TAG_SET,
        &seq(&[oid_common_name(), tlv(TAG_UTF8, common_name.as_bytes())]),
    )]);

    let validity = seq(&[utc_time(now_unix), utc_time(now_unix + days * 86_400)]);

    // v3 == INTEGER 2, explicit [0].
    let tbs = seq(&[
        explicit(0, &integer(&[2])),
        integer(serial),
        alg_sha256_rsa(),
        name.clone(),
        validity,
        name,
        spki,
    ]);

    let signing_key = rsa::pkcs1v15::SigningKey::<Sha256>::new(key.clone());
    let sig = signing_key.sign(&tbs).to_vec();

    Ok(seq(&[tbs, alg_sha256_rsa(), bit_string(&sig)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DER lengths switch form at 128 and must be minimal; a wrong length here
    /// makes a certificate that parsers reject with no useful message.
    #[test]
    fn der_lengths_use_the_right_form() {
        assert_eq!(len_bytes(0), vec![0]);
        assert_eq!(len_bytes(127), vec![127]);
        assert_eq!(len_bytes(128), vec![0x81, 128]);
        assert_eq!(len_bytes(255), vec![0x81, 255]);
        assert_eq!(len_bytes(256), vec![0x82, 0x01, 0x00]);
    }

    /// A leading high bit must be zero-padded or the integer reads negative --
    /// which is exactly how a random serial number silently becomes invalid.
    #[test]
    fn integers_are_padded_away_from_negative() {
        assert_eq!(integer(&[0x7f]), vec![0x02, 0x01, 0x7f]);
        assert_eq!(integer(&[0x80]), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(integer(&[0x00, 0x00, 0x01]), vec![0x02, 0x01, 0x01]);
    }

    /// The classic: 1.2.840.113549.1.1.11 packs its first two arcs into one
    /// byte and base-128s the rest.
    #[test]
    fn oid_encoding_matches_the_known_sha256_rsa_value() {
        assert_eq!(
            oid_sha256_with_rsa(),
            vec![0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]
        );
        assert_eq!(oid_common_name(), vec![0x06, 0x03, 0x55, 0x04, 0x03]);
    }

    #[test]
    fn civil_dates_round_trip_known_epochs() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-08-24T00:00:00Z
        assert_eq!(civil_from_unix(1_787_529_600).0, 2026);
        assert_eq!(civil_from_unix(1_787_529_600).1, 8);
    }

    /// The one that matters: a certificate this module emits must parse as a
    /// real X.509 v3 certificate, carry the CN and public key we asked for, and
    /// verify under its own signature. Hand-rolled DER that merely *looks*
    /// right is the failure mode here -- Moonlight would reject it at pairing
    /// with nothing more useful than a TLS error.
    ///
    /// 1024-bit key: this runs in an unoptimised test build and the DER shape,
    /// not the modulus size, is what is under test.
    #[test]
    fn a_generated_certificate_parses_and_self_verifies() {
        use rsa::signature::Verifier;
        use x509_parser::prelude::*;

        let mut rng = rand_core::OsRng;
        let key = RsaPrivateKey::new(&mut rng, 1024).expect("keygen");
        let der = self_signed(&key, "NVIDIA GameStream", 1_787_529_600, 3650, &[0x42; 20])
            .expect("generate");

        let (rest, cert) = X509Certificate::from_der(&der).expect("the DER must parse");
        assert!(rest.is_empty(), "trailing bytes after the certificate");
        assert_eq!(cert.version(), X509Version::V3);
        assert_eq!(
            cert.subject().iter_common_name().next().unwrap().as_str().unwrap(),
            "NVIDIA GameStream"
        );
        // Self-signed: subject and issuer are the same name.
        assert_eq!(cert.subject().to_string(), cert.issuer().to_string());
        assert!(cert.validity().not_after.timestamp() > cert.validity().not_before.timestamp());

        // The signature must actually verify over the TBS bytes.
        let vk = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(RsaPublicKey::from(&key));
        let sig = rsa::pkcs1v15::Signature::try_from(cert.signature_value.data.as_ref())
            .expect("signature bytes");
        vk.verify(cert.tbs_certificate.as_ref(), &sig)
            .expect("the certificate must verify under its own key");
    }
}
