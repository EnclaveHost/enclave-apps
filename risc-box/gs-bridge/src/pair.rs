// GameStream pairing — the 4-phase handshake, plus the store of paired
// client certificates that the HTTPS listener authenticates against.
//
// The crypto mirrors Sunshine's src/nvhttp.cpp + crypto.cpp exactly:
//
//   getservercert:        aes_key = SHA256(salt || pin)[:16]; return our cert
//   clientchallenge:      resp = AES128-ECB(SHA256(clientChal || serverCertSig
//                                || serverSecret) || serverChallenge)
//   serverchallengeresp:  return serverSecret || RSA-SHA256-sign(serverSecret)
//   clientpairingsecret:  verify SHA256(serverChal || clientCertSig || secret)
//                         == clientHash, and RSA-verify(clientCert, secret, sign)
//
// Pairing runs on plain HTTP; TLS only matters afterwards.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use openssl::hash::{hash, MessageDigest};
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode};
use openssl::x509::X509;

#[derive(Default)]
struct PairSession {
    aes_key: Option<[u8; 16]>,
    client_cert: Option<X509>,
    serversecret: Vec<u8>,
    serverchallenge: Vec<u8>,
    clienthash: Vec<u8>,
    pin: Option<String>,
}

pub struct PairState {
    cert_pem: Vec<u8>,
    x509: X509,
    pkey: PKey<Private>,
    sessions: Mutex<HashMap<String, PairSession>>,
    /// PEM of every client we have completed pairing with.
    paired: Mutex<Vec<Vec<u8>>>,
    state_dir: PathBuf,
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    openssl::rand::rand_bytes(&mut b).unwrap();
    b
}

pub fn random_hex(n: usize) -> String {
    hex(&random_bytes(n))
}

fn sha256(data: &[u8]) -> Vec<u8> {
    hash(MessageDigest::sha256(), data).unwrap().to_vec()
}

fn aes_ecb(key: &[u8], data: &[u8], mode: Mode) -> Vec<u8> {
    let mut c = Crypter::new(Cipher::aes_128_ecb(), mode, key, None).unwrap();
    c.pad(false);
    let mut out = vec![0u8; data.len() + Cipher::aes_128_ecb().block_size()];
    let mut n = c.update(data, &mut out).unwrap();
    n += c.finalize(&mut out[n..]).unwrap();
    out.truncate(n);
    out
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len().saturating_sub(1))
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn xml(fields: &[(&str, String)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root status_code=\"200\">");
    for (k, v) in fields {
        s.push_str(&format!("<{k}>{v}</{k}>"));
    }
    s.push_str("</root>");
    s
}

fn fail(msg: &str) -> String {
    eprintln!("[pair] fail: {msg}");
    format!("<?xml version=\"1.0\"?>\n<root status_code=\"400\" status_message=\"{msg}\"><paired>0</paired></root>")
}

impl PairState {
    /// Load or create the server identity and the paired-client list.
    pub fn load(state_dir: &Path) -> PairState {
        let _ = std::fs::create_dir_all(state_dir);
        let cert_path = state_dir.join("server-cert.pem");
        let key_path = state_dir.join("server-key.pem");

        let (x509, pkey) = match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            (Ok(c), Ok(k)) => match (X509::from_pem(&c), PKey::private_key_from_pem(&k)) {
                (Ok(c), Ok(k)) => (c, k),
                _ => Self::generate(&cert_path, &key_path),
            },
            _ => Self::generate(&cert_path, &key_path),
        };

        let mut paired = Vec::new();
        let paired_dir = state_dir.join("paired");
        if let Ok(entries) = std::fs::read_dir(&paired_dir) {
            for e in entries.flatten() {
                if let Ok(pem) = std::fs::read(e.path()) {
                    paired.push(pem);
                }
            }
        }
        if !paired.is_empty() {
            eprintln!("[pair] loaded {} paired client(s)", paired.len());
        }

        PairState {
            cert_pem: x509.to_pem().unwrap(),
            x509,
            pkey,
            sessions: Mutex::new(HashMap::new()),
            paired: Mutex::new(paired),
            state_dir: state_dir.to_path_buf(),
        }
    }

    fn generate(cert_path: &Path, key_path: &Path) -> (X509, PKey<Private>) {
        use openssl::asn1::Asn1Time;
        use openssl::bn::{BigNum, MsbOption};
        use openssl::x509::{X509Builder, X509NameBuilder};

        let rsa = Rsa::generate(2048).unwrap();
        let pkey = PKey::from_rsa(rsa).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "NVIDIA GameStream").unwrap();
        let name = name.build();
        let mut b = X509Builder::new().unwrap();
        b.set_version(2).unwrap();
        let mut serial = BigNum::new().unwrap();
        serial.rand(159, MsbOption::MAYBE_ZERO, false).unwrap();
        b.set_serial_number(&serial.to_asn1_integer().unwrap()).unwrap();
        b.set_subject_name(&name).unwrap();
        b.set_issuer_name(&name).unwrap();
        b.set_pubkey(&pkey).unwrap();
        b.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
        b.set_not_after(&Asn1Time::days_from_now(3650).unwrap()).unwrap();
        b.sign(&pkey, MessageDigest::sha256()).unwrap();
        let x509 = b.build();

        let _ = std::fs::write(cert_path, x509.to_pem().unwrap());
        let _ = std::fs::write(key_path, pkey.private_key_to_pem_pkcs8().unwrap());
        eprintln!("[pair] generated a new server identity");
        (x509, pkey)
    }

    pub fn server_identity(&self) -> (X509, PKey<Private>) {
        (self.x509.clone(), self.pkey.clone())
    }

    /// True if `cert` is one we paired with. Comparison is on the DER bytes,
    /// so it is exact.
    pub fn is_paired_cert(&self, cert: &X509) -> bool {
        let Ok(der) = cert.to_der() else { return false };
        self.paired.lock().unwrap().iter().any(|pem| {
            X509::from_pem(pem)
                .and_then(|c| c.to_der())
                .map(|d| d == der)
                .unwrap_or(false)
        })
    }

    fn remember(&self, cert: &X509) {
        let Ok(pem) = cert.to_pem() else { return };
        if self.is_paired_cert(cert) {
            return;
        }
        let dir = self.state_dir.join("paired");
        let _ = std::fs::create_dir_all(&dir);
        let name = hex(&sha256(&pem)[..8]);
        let _ = std::fs::write(dir.join(format!("{name}.pem")), &pem);
        self.paired.lock().unwrap().push(pem);
        eprintln!("[pair] stored client certificate {name}");
    }

    /// Forget every paired client — the client calls this when it wants to
    /// re-pair from scratch.
    pub fn unpair_all(&self) {
        self.paired.lock().unwrap().clear();
        let dir = self.state_dir.join("paired");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        self.sessions.lock().unwrap().clear();
        eprintln!("[pair] unpaired all clients");
    }

    pub fn set_pin(&self, id: &str, pin: &str) {
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .pin = Some(pin.to_string());
    }

    fn wait_for_pin(&self, id: &str, timeout: Duration) -> Option<String> {
        let start = Instant::now();
        loop {
            if let Some(p) = self.sessions.lock().unwrap().get(id).and_then(|s| s.pin.clone()) {
                return Some(p);
            }
            if start.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn cert_signature(x: &X509) -> Vec<u8> {
    x.signature().as_slice().to_vec()
}

pub fn handle(st: &PairState, args: &HashMap<String, String>) -> String {
    let id = args.get("uniqueid").cloned().unwrap_or_default();

    // The client's final confirmation, sent over HTTPS once the four phases
    // are done. Reaching it at all means the cert we stored authenticated,
    // so there is nothing left to verify. Answering anything but 200/paired=1
    // makes the client report "the PIN from the PC didn't match" and unpair.
    if args.get("phrase").map(|s| s == "pairchallenge").unwrap_or(false) {
        eprintln!("[pair] pairchallenge ok");
        return xml(&[("paired", "1".into())]);
    }

    // Phase 1: getservercert (salt + clientcert; needs the PIN)
    if args.get("phrase").map(|s| s == "getservercert").unwrap_or(false) {
        let salt = from_hex(&args.get("salt").cloned().unwrap_or_default());
        if salt.len() < 16 {
            return fail("salt too short");
        }
        st.sessions.lock().unwrap().entry(id.clone()).or_default();
        let Some(pin) = st.wait_for_pin(&id, Duration::from_secs(60)) else {
            return fail("no PIN in time");
        };
        let mut salt_pin = salt[..16].to_vec();
        salt_pin.extend_from_slice(pin.as_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&sha256(&salt_pin)[..16]);
        let cc = from_hex(&args.get("clientcert").cloned().unwrap_or_default());
        let mut sessions = st.sessions.lock().unwrap();
        let sess = sessions.entry(id.clone()).or_default();
        sess.aes_key = Some(key);
        sess.client_cert = X509::from_pem(&cc).ok();
        eprintln!("[pair] phase1 getservercert ok (client_cert={})", sess.client_cert.is_some());
        return xml(&[("paired", "1".into()), ("plaincert", hex(&st.cert_pem))]);
    }

    let mut sessions = st.sessions.lock().unwrap();
    let sess = sessions.entry(id.clone()).or_default();

    // Phase 2: clientchallenge
    if let Some(ch) = args.get("clientchallenge") {
        let Some(key) = sess.aes_key else { return fail("no key") };
        let decrypted = aes_ecb(&key, &from_hex(ch), Mode::Decrypt);
        let sig = cert_signature(&st.x509);
        let serversecret = random_bytes(16);
        let mut buf = decrypted;
        buf.extend_from_slice(&sig);
        buf.extend_from_slice(&serversecret);
        let h = sha256(&buf);
        let serverchallenge = random_bytes(16);
        let mut plaintext = h;
        plaintext.extend_from_slice(&serverchallenge);
        let encrypted = aes_ecb(&key, &plaintext, Mode::Encrypt);
        sess.serversecret = serversecret;
        sess.serverchallenge = serverchallenge;
        eprintln!("[pair] phase2 clientchallenge ok");
        return xml(&[("paired", "1".into()), ("challengeresponse", hex(&encrypted))]);
    }

    // Phase 3: serverchallengeresp
    if let Some(er) = args.get("serverchallengeresp") {
        let Some(key) = sess.aes_key else { return fail("no key") };
        sess.clienthash = aes_ecb(&key, &from_hex(er), Mode::Decrypt);
        let mut signer = Signer::new(MessageDigest::sha256(), &st.pkey).unwrap();
        signer.update(&sess.serversecret).unwrap();
        let sign = signer.sign_to_vec().unwrap();
        let mut pairingsecret = sess.serversecret.clone();
        pairingsecret.extend_from_slice(&sign);
        eprintln!("[pair] phase3 serverchallengeresp ok");
        return xml(&[("paired", "1".into()), ("pairingsecret", hex(&pairingsecret))]);
    }

    // Phase 4: clientpairingsecret
    if let Some(cps) = args.get("clientpairingsecret") {
        let cps = from_hex(cps);
        if cps.len() <= 16 {
            return fail("client pairing secret too short");
        }
        let (secret, sign) = cps.split_at(16);
        let Some(client_cert) = sess.client_cert.clone() else {
            return fail("no client cert");
        };
        let client_sig = cert_signature(&client_cert);
        let mut data = sess.serverchallenge.clone();
        data.extend_from_slice(&client_sig);
        data.extend_from_slice(secret);
        let same_hash = sha256(&data) == sess.clienthash;
        let client_pub = client_cert.public_key().unwrap();
        let mut verifier = Verifier::new(MessageDigest::sha256(), &client_pub).unwrap();
        verifier.update(secret).unwrap();
        let sig_ok = verifier.verify(sign).unwrap_or(false);

        if same_hash && sig_ok {
            eprintln!("[pair] *** PAIRED *** (hash_ok={same_hash} sig_ok={sig_ok})");
            // Drop the lock before touching the paired store.
            drop(sessions);
            st.remember(&client_cert);
            return xml(&[("paired", "1".into())]);
        }
        eprintln!("[pair] pairing FAILED (hash_ok={same_hash} sig_ok={sig_ok})");
        return fail("verification failed");
    }

    fail("unknown pair phase")
}
