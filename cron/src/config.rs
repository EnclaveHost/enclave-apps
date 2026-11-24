use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub endpoint: String,
    pub bucket: String,
    pub key: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub master_key: String,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub kind: String,
    pub url: String,
    #[serde(default = "post")]
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub users: Vec<String>,
    /// Personal Eyesoff API keys, each bound to the named user. Never a shared admin key.
    #[serde(default)]
    pub api_keys: BTreeMap<String, String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "timeout")]
    pub timeout_s: u64,
}
fn post() -> String {
    "POST".into()
}
fn timeout() -> u64 {
    600
}
fn one() -> usize {
    1
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub api_key: String,
    pub storage: Storage,
    pub targets: BTreeMap<String, Target>,
    #[serde(default = "one")]
    pub concurrency: usize,
    #[serde(default)]
    pub local_test: bool,
    #[serde(default)]
    pub sso: Option<Value>,
}
fn secret(s: &str) -> Result<String, String> {
    if let Some(name) = s.strip_prefix('$') {
        let n = name.trim_start_matches('{').trim_end_matches('}');
        std::env::var(n).map_err(|_| format!("Missing deployment secret {n}"))
    } else {
        Ok(s.into())
    }
}
impl Config {
    pub fn load() -> Result<Self, String> {
        let raw = std::env::var("ENCLAVE_CONFIG")
            .or_else(|_| std::env::var("CRON_CONFIG"))
            .map_err(|_| "Set ENCLAVE_CONFIG")?;
        Self::parse(&raw)
    }
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut c: Self =
            serde_json::from_str(raw).map_err(|e| format!("Invalid scheduler config: {e}"))?;
        if !(1..=4).contains(&c.concurrency) {
            return Err("concurrency must be 1..4".into());
        }
        c.api_key = secret(&c.api_key)?;
        c.storage.master_key = secret(&c.storage.master_key)?;
        if c.api_key.len() < 32 || c.storage.master_key.len() < 32 {
            return Err(
                "API and encryption secrets must each contain at least 32 characters".into(),
            );
        }
        c.storage.access_key = secret(&c.storage.access_key)?;
        c.storage.secret_key = secret(&c.storage.secret_key)?;
        let u = crate::net::safe_url(&c.storage.endpoint, c.local_test)?;
        if u.path() != "/" || u.query().is_some() {
            return Err("Storage endpoint must be an origin".into());
        }
        if c.storage.bucket.is_empty()
            || !c
                .storage
                .bucket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            || c.storage.key.is_empty()
            || c.storage.key.len() > 512
            || c.storage.key.split('/').any(|s| s == "." || s == "..")
        {
            return Err("Invalid storage bucket/key".into());
        }
        if !c.local_test && (c.storage.access_key.is_empty() || c.storage.secret_key.is_empty()) {
            return Err("Storage credentials required".into());
        }
        if c.targets.is_empty() || c.targets.len() > 32 {
            return Err("Configure 1..32 targets".into());
        }
        for (name, t) in &mut c.targets {
            if !valid_id(name)
                || !matches!(t.kind.as_str(), "eyesoff" | "http")
                || !matches!(
                    t.method.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE"
                )
                || !(1..=900).contains(&t.timeout_s)
            {
                return Err("Invalid target definition".into());
            }
            let u = crate::net::safe_url(&t.url, c.local_test)?;
            if t.kind == "eyesoff" && (t.method != "POST" || !u.path().ends_with("/chat")) {
                return Err("Eyesoff targets must POST to the /chat endpoint".into());
            }
            for sub in t.users.iter().chain(t.api_keys.keys()) {
                if sub != "*" && crate::sso::canonical_sub(sub).as_deref() != Some(sub.as_str()) {
                    return Err(
                        "Use canonical Enclave account IDs in target users and api_keys".into(),
                    );
                }
            }
            if t.kind == "eyesoff" && t.api_keys.contains_key("*") {
                return Err("Eyesoff credentials must be per-user".into());
            }
            for (k, v) in &mut t.headers {
                if !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    || k.is_empty()
                    || matches!(
                        k.to_ascii_lowercase().as_str(),
                        "host"
                            | "connection"
                            | "transfer-encoding"
                            | "content-length"
                            | "x-user"
                            | "idempotency-key"
                            | "x-enclave-job"
                            | "x-enclave-run"
                    )
                {
                    return Err("Invalid/reserved target header".into());
                }
                *v = secret(v)?;
                if v.contains(['\r', '\n']) {
                    return Err("Invalid header value".into());
                }
            }
            if t.kind == "eyesoff"
                && t.headers.keys().any(|k| {
                    matches!(
                        k.to_ascii_lowercase().as_str(),
                        "authorization" | "x-api-key"
                    )
                })
            {
                return Err("Eyesoff uses api_keys by user, not a shared credential header".into());
            }
            for v in t.api_keys.values_mut() {
                *v = secret(v)?;
                if v.is_empty() || v.contains(['\r', '\n']) {
                    return Err("Invalid target key".into());
                }
            }
        }
        if let Some(s) = &c.sso {
            crate::sso::SsoConfig::from_config(&json!({"sso":s}))?;
        }
        Ok(c)
    }
    pub fn target(&self, name: &str, user: &str) -> Result<&Target, String> {
        let t = self.targets.get(name).ok_or("Unknown target")?;
        if t.kind == "eyesoff" {
            if !t.api_keys.contains_key(user) {
                return Err("No personal Eyesoff key configured for this user and target".into());
            }
        } else if !t.users.iter().any(|s| s == user || s == "*") {
            return Err("Target not enabled for this user".into());
        }
        Ok(t)
    }
}
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
