//! HTTPS for `.eth`: no public CA can issue a certificate for an ENS name,
//! so this machine has its own. The CA is made on first start in the
//! service's private state directory; its key never leaves it. An X.509
//! name constraint limits it to names under `eth` and `ens`, so even a
//! leaked key could not vouch for any other site. Each name gets a
//! certificate on demand, kept in memory.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyPair, KeyUsagePurpose,
    NameConstraints,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};

pub const CA_CERT: &str = "ca.crt";
const CA_KEY: &str = "ca.key";

pub struct Ca {
    cert: rcgen::Certificate,
    key: KeyPair,
    leaves: Mutex<HashMap<String, Arc<CertifiedKey>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ca")
    }
}

fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    p.distinguished_name.push(DnType::CommonName, "EnclaveDNS Local CA (this machine only, .eth and .ens names)");
    p.distinguished_name.push(DnType::OrganizationName, "EnclaveDNS Local");
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName("eth".into()), GeneralSubtree::DnsName("ens".into())],
        excluded_subtrees: vec![],
    });
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::days(1);
    p.not_after = now + Duration::days(3650);
    p
}

impl Ca {
    /// The CA in `dir`, made there first if it does not exist yet.
    pub fn load_or_create(dir: &Path) -> Result<Ca, String> {
        let (cp, kp) = (dir.join(CA_CERT), dir.join(CA_KEY));
        let (cert, key) = if cp.exists() && kp.exists() {
            let key_pem = std::fs::read_to_string(&kp).map_err(|e| format!("{}: {e}", kp.display()))?;
            let cert_pem = std::fs::read_to_string(&cp).map_err(|e| format!("{}: {e}", cp.display()))?;
            let key = KeyPair::from_pem(&key_pem).map_err(|e| format!("{}: {e}", kp.display()))?;
            // the same subject and key as the trusted certificate, so what it
            // signs chains to it
            let params = CertificateParams::from_ca_cert_pem(&cert_pem).map_err(|e| format!("{}: {e}", cp.display()))?;
            let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
            (cert, key)
        } else {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let key = KeyPair::generate().map_err(|e| e.to_string())?;
            let cert = ca_params().self_signed(&key).map_err(|e| e.to_string())?;
            write_private(&kp, key.serialize_pem().as_bytes())?;
            std::fs::write(&cp, cert.pem()).map_err(|e| format!("{}: {e}", cp.display()))?;
            eprintln!("enclavedns-local: made a new .eth CA at {} (trust that certificate to get HTTPS)", cp.display());
            (cert, key)
        };
        Ok(Ca { cert, key, leaves: Mutex::default(), provider: Arc::new(rustls::crypto::ring::default_provider()) })
    }

    fn leaf(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        if let Some(k) = self.leaves.lock().unwrap().get(name) {
            return Some(k.clone());
        }
        let mut p = CertificateParams::new(vec![name.to_string()]).ok()?;
        p.distinguished_name.push(DnType::CommonName, name);
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.use_authority_key_identifier_extension = true;
        let now = OffsetDateTime::now_utc();
        p.not_before = now - Duration::days(1);
        p.not_after = now + Duration::days(90);
        let key = KeyPair::generate().ok()?;
        let cert = p.signed_by(&key, &self.cert, &self.key).ok()?;
        let der_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = self.provider.key_provider.load_private_key(der_key).ok()?;
        let ck = Arc::new(CertifiedKey::new(vec![CertificateDer::from(cert.der().to_vec()), CertificateDer::from(self.cert.der().to_vec())], signing));
        let mut m = self.leaves.lock().unwrap();
        if m.len() > 4096 {
            m.clear();
        }
        m.insert(name.to_string(), ck.clone());
        Some(ck)
    }

    pub fn server_config(self: Arc<Self>) -> Arc<rustls::ServerConfig> {
        let provider = self.provider.clone();
        let mut c = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("tls versions")
            .with_no_client_auth()
            .with_cert_resolver(self);
        c.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(c)
    }
}

impl ResolvesServerCert for Ca {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        let ok = (name.ends_with(".eth") || name.ends_with(".ens"))
            && name.len() <= 253
            && name.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        if !ok {
            return None;
        }
        self.leaf(&name)
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(data).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_round_trip_and_constraint() {
        let dir = std::env::temp_dir().join(format!("enclavedns-local-ca-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Ca::load_or_create(&dir).unwrap();
        let first = std::fs::read_to_string(dir.join(CA_CERT)).unwrap();
        assert!(a.leaf("playhh.eth").is_some());
        // a restart reuses the same CA, it does not make a new one
        let b = Ca::load_or_create(&dir).unwrap();
        assert_eq!(first, std::fs::read_to_string(dir.join(CA_CERT)).unwrap());
        assert!(b.leaf("vitalik.eth").is_some());
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(dir.join(CA_KEY)).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
