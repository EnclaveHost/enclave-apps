// GameStream host — the thing a real Moonlight client discovers and pairs
// with. Native Rust prototype (fast to iterate against the real client; also
// the right long-term shape — a native bridge that pulls the RISC Box app's
// AV1 /video and posts input to /hid, running where a GPU is reachable).
//
// Crypto mirrors Sunshine's src/nvhttp.cpp + crypto.cpp exactly.
// Pairing is plain HTTP on :47989 (TLS only matters post-pair).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openssl::hash::{hash, MessageDigest};
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode};
use openssl::x509::X509;

const PORT_HTTP: u16 = 47989;
const PORT_HTTPS: u16 = 47984;
const APP_VERSION: &str = "7.1.431.0"; // the GFE version Moonlight expects
const GFE_VERSION: &str = "3.23.0.74";
const UNIQUE_ID: &str = "0123456789ABCDEF";

struct ServerId {
    cert_pem: Vec<u8>,
    x509: X509,
    pkey: PKey<openssl::pkey::Private>,
}

#[derive(Default)]
struct PairSession {
    aes_key: Option<[u8; 16]>,
    client_cert: Option<X509>,
    serversecret: Vec<u8>,
    serverchallenge: Vec<u8>,
    clienthash: Vec<u8>,
    pin: Option<String>,
}

struct State {
    id: ServerId,
    sessions: Mutex<HashMap<String, PairSession>>,
    paired: Mutex<bool>,
}

fn gen_self_signed() -> ServerId {
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
    let cert_pem = x509.to_pem().unwrap();
    ServerId { cert_pem, x509, pkey }
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

fn cert_signature(x: &X509) -> Vec<u8> {
    x.signature().as_slice().to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}
fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len().saturating_sub(1)).step_by(2)
        .filter_map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&').filter_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        Some((k.to_string(), urldecode(v)))
    }).collect()
}
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) { out.push(v as char); i += 3; continue; }
        }
        out.push(b[i] as char); i += 1;
    }
    out
}

fn xml(fields: &[(&str, String)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<root status_code=\"200\">");
    for (k, v) in fields { s.push_str(&format!("<{k}>{v}</{k}>")); }
    s.push_str("</root>");
    s
}

fn serverinfo(paired: bool) -> String {
    xml(&[
        ("hostname", "RISC Box".into()),
        ("appversion", APP_VERSION.into()),
        ("GfeVersion", GFE_VERSION.into()),
        ("uniqueid", UNIQUE_ID.into()),
        ("HttpsPort", PORT_HTTPS.to_string()),
        ("ExternalPort", PORT_HTTP.to_string()),
        ("MaxLumaPixelsHEVC", "0".into()),
        ("mac", "00:00:00:00:00:00".into()),
        ("LocalIP", "127.0.0.1".into()),
        ("ServerCodecModeSupport", "259".into()),
        ("PairStatus", if paired { "1" } else { "0" }.into()),
        ("currentgame", "0".into()),
        ("state", "SUNSHINE_SERVER_FREE".into()),
    ])
}

fn rand16() -> Vec<u8> {
    let mut b = vec![0u8; 16];
    openssl::rand::rand_bytes(&mut b).unwrap();
    b
}

fn wait_for_pin(st: &State, id: &str, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    loop {
        if let Some(p) = st.sessions.lock().unwrap().get(id).and_then(|s| s.pin.clone()) {
            return Some(p);
        }
        if start.elapsed() > timeout { return None; }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn fail(msg: &str) -> String {
    eprintln!("[gshost] pair fail: {msg}");
    format!("<?xml version=\"1.0\"?>\n<root status_code=\"400\" status_message=\"{msg}\"><paired>0</paired></root>")
}

fn handle_pair(st: &State, args: &HashMap<String, String>) -> String {
    let id = args.get("uniqueid").cloned().unwrap_or_default();

    // Phase 1: getservercert (salt + clientcert; needs the PIN)
    if args.get("phrase").map(|s| s == "getservercert").unwrap_or(false) {
        let salt = from_hex(&args.get("salt").cloned().unwrap_or_default());
        if salt.len() < 16 { return fail("salt too short"); }
        st.sessions.lock().unwrap().entry(id.clone()).or_default();
        let Some(pin) = wait_for_pin(st, &id, Duration::from_secs(60)) else { return fail("no PIN in time"); };
        let mut salt_pin = salt[..16].to_vec();
        salt_pin.extend_from_slice(pin.as_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&sha256(&salt_pin)[..16]);
        let cc = from_hex(&args.get("clientcert").cloned().unwrap_or_default());
        let mut sessions = st.sessions.lock().unwrap();
        let sess = sessions.entry(id.clone()).or_default();
        sess.aes_key = Some(key);
        sess.client_cert = X509::from_pem(&cc).ok();
        eprintln!("[gshost] phase1 getservercert ok (client_cert={})", sess.client_cert.is_some());
        return xml(&[("paired", "1".into()), ("plaincert", hex(&st.id.cert_pem))]);
    }

    let mut sessions = st.sessions.lock().unwrap();
    let sess = sessions.entry(id.clone()).or_default();

    // Phase 2: clientchallenge
    if let Some(ch) = args.get("clientchallenge") {
        let Some(key) = sess.aes_key else { return fail("no key"); };
        let decrypted = aes_ecb(&key, &from_hex(ch), Mode::Decrypt);
        let sig = cert_signature(&st.id.x509);
        let serversecret = rand16();
        let mut buf = decrypted;
        buf.extend_from_slice(&sig);
        buf.extend_from_slice(&serversecret);
        let h = sha256(&buf);
        let serverchallenge = rand16();
        let mut plaintext = h;
        plaintext.extend_from_slice(&serverchallenge);
        let encrypted = aes_ecb(&key, &plaintext, Mode::Encrypt);
        sess.serversecret = serversecret;
        sess.serverchallenge = serverchallenge;
        eprintln!("[gshost] phase2 clientchallenge ok");
        return xml(&[("paired", "1".into()), ("challengeresponse", hex(&encrypted))]);
    }

    // Phase 3: serverchallengeresp
    if let Some(er) = args.get("serverchallengeresp") {
        let Some(key) = sess.aes_key else { return fail("no key"); };
        sess.clienthash = aes_ecb(&key, &from_hex(er), Mode::Decrypt);
        let mut signer = Signer::new(MessageDigest::sha256(), &st.id.pkey).unwrap();
        signer.update(&sess.serversecret).unwrap();
        let sign = signer.sign_to_vec().unwrap();
        let mut pairingsecret = sess.serversecret.clone();
        pairingsecret.extend_from_slice(&sign);
        eprintln!("[gshost] phase3 serverchallengeresp ok");
        return xml(&[("paired", "1".into()), ("pairingsecret", hex(&pairingsecret))]);
    }

    // Phase 4: clientpairingsecret
    if let Some(cps) = args.get("clientpairingsecret") {
        let cps = from_hex(cps);
        if cps.len() <= 16 { return fail("client pairing secret too short"); }
        let (secret, sign) = cps.split_at(16);
        let Some(client_cert) = sess.client_cert.clone() else { return fail("no client cert"); };
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
            *st.paired.lock().unwrap() = true;
            eprintln!("[gshost] *** PAIRED *** (hash_ok={same_hash} sig_ok={sig_ok})");
            return xml(&[("paired", "1".into())]);
        }
        eprintln!("[gshost] pairing FAILED (hash_ok={same_hash} sig_ok={sig_ok})");
        return fail("verification failed");
    }

    fail("unknown pair phase")
}

fn respond(mut stream: TcpStream, body: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn main() {
    let st = Arc::new(State { id: gen_self_signed(), sessions: Mutex::new(HashMap::new()), paired: Mutex::new(false) });
    let listener = TcpListener::bind(("0.0.0.0", PORT_HTTP)).unwrap();
    eprintln!("[gshost] GameStream host on :{PORT_HTTP} (uniqueid {UNIQUE_ID})");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let st = st.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or("");
            let path = line.split_whitespace().nth(1).unwrap_or("/");
            let (route, query) = path.split_once('?').unwrap_or((path, ""));
            let args = parse_query(query);
            eprintln!("[gshost] > {route}");
            let paired = *st.paired.lock().unwrap();
            let body = match route {
                "/serverinfo" => serverinfo(paired),
                "/pair" => handle_pair(&st, &args),
                "/pin" => {
                    let id = args.get("uniqueid").cloned().unwrap_or_default();
                    let pin = args.get("pin").cloned().unwrap_or_default();
                    st.sessions.lock().unwrap().entry(id).or_default().pin = Some(pin);
                    "<?xml version=\"1.0\"?><root status_code=\"200\"><pin>ok</pin></root>".into()
                }
                _ => xml(&[]),
            };
            respond(stream, &body);
        });
    }
}
