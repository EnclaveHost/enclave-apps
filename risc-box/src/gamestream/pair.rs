//! GameStream pairing: the four-phase challenge that ends with Moonlight
//! trusting this host's certificate.
//!
//! Ported from the native bridge with three changes forced by the sandbox:
//!
//!   * **Crypto is RustCrypto.** openssl is C and does not cross-compile.
//!   * **Nothing blocks.** The bridge parked a thread in `wait_for_pin` for up
//!     to 60 s; in-guest there are no threads (`std::thread::spawn` is os error
//!     58 on wasip2) and the caller is the same loop that steps the emulator,
//!     so a blocking wait would freeze the machine. Phase 1 returns
//!     [`Outcome::AwaitPin`] and the poll loop holds the connection open.
//!   * **State lives in object storage,** not a state directory: the guest has
//!     no writable filesystem. Pairing that survives only until the next
//!     restart is pairing that has to be redone every restart, and this app
//!     restarts often.

use std::collections::HashMap;
use std::sync::Mutex;

use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

use crate::gamestream::crypto::{ecb_decrypt_block, ecb_encrypt_block};
use crate::gamestream::x509gen;

/// Where the server identity and the paired-client list live. Abstracted so
/// the handshake can be tested without object storage in the loop.
/// Durable storage for the host identity and the paired-client list.
///
/// Deliberately just get/put on whole values: the backing store is S3, which
/// this app reaches through `crate::s3`, and that module has no list operation.
/// So the paired set is ONE blob rather than a key per client -- fewer round
/// trips, and no enumeration to emulate.
pub trait Store: Send + Sync {
    fn get(&self, key: &str) -> Option<Vec<u8>>;
    fn put(&self, key: &str, value: &[u8]);
}

/// A store that forgets on restart. Correct for tests; for the real host it
/// means re-pairing every restart, so the S3-backed store is what ships.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);

impl Store for MemoryStore {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned()
    }
    fn put(&self, key: &str, value: &[u8]) {
        self.0.lock().unwrap().insert(key.to_string(), value.to_vec());
    }
}

const KEY_IDENTITY: &str = "gamestream/server-key.der";
const KEY_CERT: &str = "gamestream/server-cert.der";
const KEY_PAIRED: &str = "gamestream/paired.json";

pub fn random_bytes(n: usize) -> Vec<u8> {
    use rand_core::RngCore;
    let mut b = vec![0u8; n];
    rand_core::OsRng.fill_bytes(&mut b);
    b
}

pub fn random_hex(n: usize) -> String {
    hex(&random_bytes(n))
}

fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len() / 2)
        .filter_map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

fn xml(fields: &[(&str, String)]) -> String {
    let body: String = fields.iter().map(|(k, v)| format!("<{k}>{v}</{k}>")).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <root status_code=\"200\">{body}</root>"
    )
}

fn fail(msg: &str) -> String {
    eprintln!("[pair] {msg}");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <root status_code=\"400\" status_message=\"{msg}\"><paired>0</paired></root>"
    )
}

/// A uniqueid is client-supplied and becomes a storage key, so reduce it to
/// something that cannot escape the prefix. Everything outside [A-Za-z0-9]
/// goes and the result is bounded.
fn safe_id(id: &str) -> String {
    let s: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).take(64).collect();
    if s.is_empty() { "anon".into() } else { s }
}

/// A client certificate's signature bytes, which both ends hash into the
/// challenge. Parsed rather than reconstructed: the client chose the encoding.
fn cert_signature(der: &[u8]) -> Option<Vec<u8>> {
    use x509_parser::prelude::*;
    let (_, c) = X509Certificate::from_der(der).ok()?;
    Some(c.signature_value.data.to_vec())
}

/// The RSA public key out of a certificate, for verifying phase 4.
fn cert_public_key(der: &[u8]) -> Option<RsaPublicKey> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;
    use x509_parser::prelude::*;
    let (_, c) = X509Certificate::from_der(der).ok()?;
    let spki = c.public_key();
    // Try SPKI first (what a normal certificate carries), then a bare PKCS#1
    // key, because Moonlight's own certificate has been seen both ways.
    RsaPublicKey::from_public_key_der(spki.raw)
        .ok()
        .or_else(|| RsaPublicKey::from_pkcs1_der(spki.subject_public_key.data.as_ref()).ok())
}

/// PEM -> DER for the certificate the client sends. Hand-rolled because the
/// only thing needed is one base64 body between two markers.
fn pem_to_der(pem: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(pem).ok()?;
    let body: String = text
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    b64_decode(&body)
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in s.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() {
            continue;
        }
        let v = T.iter().position(|&t| t == ch)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[derive(Default)]
struct PairSession {
    aes_key: Option<[u8; 16]>,
    client_cert: Option<Vec<u8>>,
    serversecret: Vec<u8>,
    serverchallenge: Vec<u8>,
    clienthash: Vec<u8>,
    /// The PIN, once the operator supplies it out of band.
    pin: Option<String>,
}

/// What the caller should do with a pairing request.
pub enum Outcome {
    /// Send this body back now.
    Reply(String),
    /// Phase 1 with no PIN yet: hold the connection open and ask again on a
    /// later poll. The bridge blocked a thread here; the guest cannot.
    AwaitPin,
}

pub struct PairState {
    cert_der: Vec<u8>,
    key: RsaPrivateKey,
    sessions: Mutex<HashMap<String, PairSession>>,
    paired: Mutex<HashMap<String, Vec<u8>>>,
    store: Box<dyn Store>,
}

impl PairState {
    /// Load the server identity, generating one on first run.
    ///
    /// `now_unix` is passed in rather than read here so the caller owns the
    /// clock (the guest's is the host's, via the emulator's realtime source).
    pub fn load(store: Box<dyn Store>, now_unix: i64) -> PairState {
        let existing = match (store.get(KEY_IDENTITY), store.get(KEY_CERT)) {
            (Some(k), Some(c)) => RsaPrivateKey::from_pkcs1_der(&k).ok().map(|k| (k, c)),
            _ => None,
        };
        let (key, cert_der) = match existing {
            Some(pair) => {
                eprintln!("[pair] loaded the stored server identity");
                pair
            }
            None => {
                eprintln!("[pair] generating a server identity (RSA-2048, ~seconds)");
                let mut rng = rand_core::OsRng;
                let key = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
                let cert = x509gen::self_signed(
                    &key,
                    "NVIDIA GameStream",
                    now_unix,
                    3650,
                    &random_bytes(20),
                )
                .expect("self-signed certificate");
                if let Ok(der) = key.to_pkcs1_der() {
                    store.put(KEY_IDENTITY, der.as_bytes());
                }
                store.put(KEY_CERT, &cert);
                eprintln!("[pair] generated a new server identity");
                (key, cert)
            }
        };

        let paired = store
            .get(KEY_PAIRED)
            .and_then(|b| decode_paired(&b))
            .unwrap_or_default();
        if !paired.is_empty() {
            eprintln!("[pair] loaded {} paired client(s)", paired.len());
        }

        PairState {
            cert_der,
            key,
            sessions: Mutex::new(HashMap::new()),
            paired: Mutex::new(paired),
            store,
        }
    }

    /// The serving certificate, DER. The HTTPS surface serves under it and
    /// phase 1 hands it to the client.
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub fn private_key(&self) -> &RsaPrivateKey {
        &self.key
    }

    /// The operator supplies the PIN out of band (the app's own UI), which
    /// releases a phase-1 request parked in [`Outcome::AwaitPin`].
    pub fn submit_pin(&self, id: &str, pin: &str) {
        let mut s = self.sessions.lock().unwrap();
        s.entry(id.to_string()).or_default().pin = Some(pin.to_string());
        eprintln!("[pair] PIN supplied for {}", safe_id(id));
    }

    /// True if this exact certificate is one we completed pairing with.
    /// Compared on DER bytes, so it is exact.
    pub fn is_paired_der(&self, der: &[u8]) -> bool {
        self.paired.lock().unwrap().values().any(|d| d == der)
    }

    /// Forget ONE client's pairing.
    ///
    /// Scoped deliberately. This endpoint is unauthenticated plain HTTP and
    /// Moonlight calls it by itself whenever it cannot verify a host, so
    /// wiping every pairing here would let one confused client log everyone
    /// else out.
    pub fn unpair(&self, id: &str) {
        let key = safe_id(id);
        self.sessions.lock().unwrap().remove(id);
        let gone = self.paired.lock().unwrap().remove(&key).is_some();
        if gone {
            self.flush_paired();
            eprintln!("[pair] unpaired {key}");
        } else {
            eprintln!("[pair] /unpair for unknown client {key}; nothing to do");
        }
    }

    fn remember(&self, id: &str, cert_der: &[u8]) {
        let key = safe_id(id);
        self.paired.lock().unwrap().insert(key.clone(), cert_der.to_vec());
        self.flush_paired();
        eprintln!("[pair] stored client certificate for {key}");
    }

    /// Write the whole paired set back. Small (one certificate per client we
    /// have ever paired with) and rare (only on pair/unpair).
    fn flush_paired(&self) {
        let blob = encode_paired(&self.paired.lock().unwrap());
        self.store.put(KEY_PAIRED, &blob);
    }

    /// One step of the handshake. Never blocks.
    pub fn handle(&self, args: &HashMap<String, String>) -> Outcome {
        let id = args.get("uniqueid").cloned().unwrap_or_default();
        let phrase = args.get("phrase").map(String::as_str).unwrap_or("");

        // The client's final confirmation, over HTTPS once the four phases are
        // done. Reaching it means the stored certificate authenticated, so
        // there is nothing left to verify. Anything but 200/paired=1 makes the
        // client report a PIN mismatch and unpair.
        if phrase == "pairchallenge" {
            eprintln!("[pair] pairchallenge ok");
            return Outcome::Reply(xml(&[("paired", "1".into())]));
        }

        // Phase 1: getservercert (salt + clientcert; needs the PIN).
        if phrase == "getservercert" {
            let salt = from_hex(args.get("salt").map(String::as_str).unwrap_or(""));
            if salt.len() < 16 {
                return Outcome::Reply(fail("salt too short"));
            }
            let pin = {
                let mut sessions = self.sessions.lock().unwrap();
                sessions.entry(id.clone()).or_default().pin.clone()
            };
            let Some(pin) = pin else { return Outcome::AwaitPin };

            let mut salt_pin = salt[..16].to_vec();
            salt_pin.extend_from_slice(pin.as_bytes());
            let mut key = [0u8; 16];
            key.copy_from_slice(&sha256(&salt_pin)[..16]);

            let cc = from_hex(args.get("clientcert").map(String::as_str).unwrap_or(""));
            let client_der = pem_to_der(&cc);
            let mut sessions = self.sessions.lock().unwrap();
            let sess = sessions.entry(id.clone()).or_default();
            sess.aes_key = Some(key);
            sess.client_cert = client_der;
            eprintln!(
                "[pair] phase1 getservercert ok (client_cert={})",
                sess.client_cert.is_some()
            );
            return Outcome::Reply(xml(&[
                ("paired", "1".into()),
                ("plaincert", hex(&pem_wrap(&self.cert_der))),
            ]));
        }

        let mut sessions = self.sessions.lock().unwrap();
        let sess = sessions.entry(id.clone()).or_default();

        // Phase 2: clientchallenge.
        if let Some(ch) = args.get("clientchallenge") {
            let Some(key) = sess.aes_key else { return Outcome::Reply(fail("no key")) };
            let decrypted = ecb_decrypt_block(&key, &from_hex(ch));
            let Some(sig) = cert_signature(&self.cert_der) else {
                return Outcome::Reply(fail("own certificate unreadable"));
            };
            let serversecret = random_bytes(16);
            let mut buf = decrypted;
            buf.extend_from_slice(&sig);
            buf.extend_from_slice(&serversecret);
            let h = sha256(&buf);
            let serverchallenge = random_bytes(16);
            let mut plaintext = h;
            plaintext.extend_from_slice(&serverchallenge);
            let encrypted = ecb_encrypt_block(&key, &plaintext);
            sess.serversecret = serversecret;
            sess.serverchallenge = serverchallenge;
            eprintln!("[pair] phase2 clientchallenge ok");
            return Outcome::Reply(xml(&[
                ("paired", "1".into()),
                ("challengeresponse", hex(&encrypted)),
            ]));
        }

        // Phase 3: serverchallengeresp.
        if let Some(er) = args.get("serverchallengeresp") {
            let Some(key) = sess.aes_key else { return Outcome::Reply(fail("no key")) };
            sess.clienthash = ecb_decrypt_block(&key, &from_hex(er));
            let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(self.key.clone());
            let sign = signer.sign(&sess.serversecret).to_vec();
            let mut pairingsecret = sess.serversecret.clone();
            pairingsecret.extend_from_slice(&sign);
            eprintln!("[pair] phase3 serverchallengeresp ok");
            return Outcome::Reply(xml(&[
                ("paired", "1".into()),
                ("pairingsecret", hex(&pairingsecret)),
            ]));
        }

        // Phase 4: clientpairingsecret.
        if let Some(cps) = args.get("clientpairingsecret") {
            let cps = from_hex(cps);
            if cps.len() <= 16 {
                return Outcome::Reply(fail("client pairing secret too short"));
            }
            let (secret, sign) = cps.split_at(16);
            let Some(client_der) = sess.client_cert.clone() else {
                return Outcome::Reply(fail("no client cert"));
            };
            let Some(client_sig) = cert_signature(&client_der) else {
                return Outcome::Reply(fail("client certificate unreadable"));
            };
            let mut data = sess.serverchallenge.clone();
            data.extend_from_slice(&client_sig);
            data.extend_from_slice(secret);
            let same_hash = sha256(&data) == sess.clienthash;

            let sig_ok = cert_public_key(&client_der)
                .and_then(|pk| {
                    let vk = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(pk);
                    let s = rsa::pkcs1v15::Signature::try_from(sign).ok()?;
                    Some(vk.verify(secret, &s).is_ok())
                })
                .unwrap_or(false);

            if same_hash && sig_ok {
                eprintln!("[pair] *** PAIRED *** (hash_ok={same_hash} sig_ok={sig_ok})");
                drop(sessions);
                self.remember(&id, &client_der);
                return Outcome::Reply(xml(&[("paired", "1".into())]));
            }
            eprintln!("[pair] pairing FAILED (hash_ok={same_hash} sig_ok={sig_ok})");
            return Outcome::Reply(fail("verification failed"));
        }

        Outcome::Reply(fail("unknown pair phase"))
    }
}

/// The client expects `plaincert` as hex-encoded PEM, not hex-encoded DER.
fn pem_wrap(der: &[u8]) -> Vec<u8> {
    let b64 = b64_encode(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out.into_bytes()
}

fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed a store with a small identity so `load` takes the persisted path:
    /// RSA-2048 keygen in an unoptimised test build is seconds, and what is
    /// under test here is the storage round trip, not the modulus.
    fn seeded() -> (MemoryStore, Vec<u8>) {
        let mut rng = rand_core::OsRng;
        let key = RsaPrivateKey::new(&mut rng, 1024).expect("keygen");
        let cert = x509gen::self_signed(&key, "NVIDIA GameStream", 1_787_529_600, 3650, &[9; 20])
            .expect("cert");
        let store = MemoryStore::default();
        store.put(KEY_IDENTITY, key.to_pkcs1_der().unwrap().as_bytes());
        store.put(KEY_CERT, &cert);
        (store, cert)
    }

    /// The identity must survive a restart. If it does not, Moonlight sees a
    /// different certificate than the one it pinned and every client has to
    /// pair again -- which, on an app that restarts as often as this one, is
    /// the difference between "works" and "unusable".
    #[test]
    fn the_server_identity_survives_a_reload() {
        let (store, cert) = seeded();
        let a = PairState::load(Box::new(store), 1_787_529_600);
        assert_eq!(a.cert_der(), &cert[..], "reload must reuse the stored certificate");
    }

    /// Phase 1 must NOT block waiting for the PIN -- the caller is the loop
    /// that steps the emulator. It reports AwaitPin, and once the PIN lands the
    /// same request answers.
    #[test]
    fn phase_one_defers_until_the_pin_arrives_instead_of_blocking() {
        let (store, _) = seeded();
        let st = PairState::load(Box::new(store), 1_787_529_600);
        let mut args = HashMap::new();
        args.insert("phrase".to_string(), "getservercert".to_string());
        args.insert("uniqueid".to_string(), "testclient".to_string());
        args.insert("salt".to_string(), hex(&[0xABu8; 16]));
        args.insert("clientcert".to_string(), String::new());

        assert!(
            matches!(st.handle(&args), Outcome::AwaitPin),
            "with no PIN yet the request must be parked, not answered or blocked"
        );

        st.submit_pin("testclient", "1234");
        match st.handle(&args) {
            Outcome::Reply(body) => {
                assert!(body.contains("<paired>1</paired>"), "got: {body}");
                assert!(body.contains("plaincert"), "the client needs our certificate");
            }
            Outcome::AwaitPin => panic!("the PIN was supplied; this must answer"),
        }
    }

    /// A short salt is a malformed request, not a reason to derive a key from
    /// out-of-bounds bytes.
    #[test]
    fn a_short_salt_is_rejected() {
        let (store, _) = seeded();
        let st = PairState::load(Box::new(store), 1_787_529_600);
        let mut args = HashMap::new();
        args.insert("phrase".to_string(), "getservercert".to_string());
        args.insert("salt".to_string(), "aabb".to_string());
        match st.handle(&args) {
            Outcome::Reply(b) => assert!(b.contains("status_code=\"400\""), "got: {b}"),
            Outcome::AwaitPin => panic!("a malformed salt must fail, not wait for a PIN"),
        }
    }

    /// The client is handed hex-encoded PEM, and our own PEM must parse back to
    /// the DER we started from -- the wrapper is easy to get subtly wrong and
    /// the failure shows up as an unusable certificate at the far end.
    #[test]
    fn the_pem_wrapper_round_trips() {
        let (_, cert) = seeded();
        let pem = pem_wrap(&cert);
        assert_eq!(pem_to_der(&pem).as_deref(), Some(&cert[..]));
    }

    /// Pairing must survive a restart, and it now rides in one blob -- so the
    /// blob is the thing that has to round-trip. A silent failure here means
    /// every client re-pairs after each restart, which is exactly the symptom
    /// that makes a streaming host feel broken.
    #[test]
    fn the_paired_set_round_trips_through_storage() {
        let (store, _) = seeded();
        let store = Box::new(store);
        let st = PairState::load(store, 1_787_529_600);
        let cert = vec![0x30, 0x82, 0x01, 0x0a, 0xde, 0xad, 0xbe, 0xef];
        st.remember("moonlight-client", &cert);
        assert!(st.is_paired_der(&cert));

        // Re-encode/decode the way a restart would.
        let blob = encode_paired(&st.paired.lock().unwrap());
        let back = decode_paired(&blob).expect("the blob must decode");
        assert_eq!(back.get("moonlightclient"), Some(&cert), "got: {back:?}");

        st.unpair("moonlight-client");
        assert!(!st.is_paired_der(&cert), "unpair must actually forget");
    }

    /// A client-supplied id becomes a storage key, so it must not be able to
    /// escape the prefix.
    #[test]
    fn a_hostile_uniqueid_cannot_escape_the_prefix() {
        assert_eq!(safe_id("../../etc/passwd"), "etcpasswd");
        assert_eq!(safe_id(""), "anon");
        assert_eq!(safe_id("a/b\\c").len(), 3);
        assert!(safe_id(&"x".repeat(500)).len() <= 64);
    }
}

/// The paired set on the wire: a JSON object of uniqueid -> hex DER. JSON
/// because the app already carries serde_json, hex because certificates are
/// binary and this file is read by humans when pairing misbehaves.
fn encode_paired(map: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    let obj: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(hex(v))))
        .collect();
    serde_json::to_vec(&serde_json::Value::Object(obj)).unwrap_or_default()
}

fn decode_paired(blob: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    let v: serde_json::Value = serde_json::from_slice(blob).ok()?;
    let obj = v.as_object()?;
    let mut out = HashMap::new();
    for (k, val) in obj {
        if let Some(h) = val.as_str() {
            let der = from_hex(h);
            if !der.is_empty() {
                out.insert(k.clone(), der);
            }
        }
    }
    Some(out)
}

/// The shipping [`Store`]: pairing state in the same bucket the machine images
/// come from.
///
/// The guest has no writable filesystem, and the identity is precisely what
/// Moonlight pins at pairing time — so losing it on restart means every client
/// has to pair again. This app restarts often enough that that is the
/// difference between usable and not.
pub struct S3Store {
    ep: crate::s3::Endpoint,
    bucket: String,
    creds: Option<crate::s3::Creds>,
    /// Read-through cache. A restart reloads from S3; within one run the
    /// values never change underneath us, so a hit avoids a network round trip
    /// on a path that would otherwise sit in the emulator's turn.
    cache: Mutex<HashMap<String, Vec<u8>>>,
}

impl S3Store {
    pub fn new(
        ep: crate::s3::Endpoint,
        bucket: String,
        creds: Option<crate::s3::Creds>,
    ) -> S3Store {
        S3Store { ep, bucket, creds, cache: Mutex::new(HashMap::new()) }
    }
}

impl Store for S3Store {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        if let Some(v) = self.cache.lock().unwrap().get(key) {
            return Some(v.clone());
        }
        let mut noop = |_: usize, _: usize| {};
        match crate::s3::get_object(&self.ep, &self.bucket, key, self.creds.as_ref(), &mut noop) {
            Ok(v) => {
                self.cache.lock().unwrap().insert(key.to_string(), v.clone());
                Some(v)
            }
            // A missing key is the ordinary first-run case, not an error worth
            // shouting about; anything else is worth one line.
            Err(e) => {
                if !e.contains("404") && !e.contains("NoSuchKey") {
                    eprintln!("[pair] store read {key}: {e}");
                }
                None
            }
        }
    }

    fn put(&self, key: &str, value: &[u8]) {
        self.cache.lock().unwrap().insert(key.to_string(), value.to_vec());
        if let Err(e) =
            crate::s3::put_object(&self.ep, &self.bucket, key, self.creds.as_ref(), value)
        {
            // In-memory state stays correct for this run; say plainly that it
            // will not survive a restart, because that is the failure the
            // operator will otherwise meet as "I have to pair again".
            eprintln!("[pair] WARNING could not persist {key}: {e} (pairing will not survive a restart)");
        }
    }
}
