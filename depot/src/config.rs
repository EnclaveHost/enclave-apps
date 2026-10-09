//! The app config (ENCLAVE_CONFIG): storage, the master key, users and
//! their permissions, public repositories, protected refs, limits. Values
//! written `$NAME` are deployment secrets, read from the environment; the
//! config itself is public, so it may hold token *hashes* but never tokens.

use crate::git::glob;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub endpoint: String,
    #[serde(default = "auto")]
    pub region: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
}

fn auto() -> String {
    "auto".into()
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct User {
    /// `$SECRET` holding the token, or (local tests only) the token itself
    #[serde(default)]
    pub token: Option<String>,
    /// hex SHA-256 of the token: safe to publish in the config
    #[serde(default)]
    pub token_sha256: Option<String>,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    /// an Enclave account (acct_… or a wallet address) that signs in as this user
    #[serde(default)]
    pub account: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    storage: Storage,
    master_key: String,
    #[serde(default)]
    users: BTreeMap<String, User>,
    #[serde(default)]
    public: Vec<String>,
    #[serde(default)]
    protected: Vec<String>,
    #[serde(default = "main")]
    default_branch: String,
    #[serde(default)]
    max_push_mb: Option<u64>,
    #[serde(default = "object_mb")]
    max_object_mb: u64,
    #[serde(default)]
    cache_mb: Option<usize>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    local_test: bool,
    #[serde(default)]
    sso: Option<serde_json::Value>,
    #[serde(default)]
    hooks: Vec<crate::hooks::Hook>,
    #[serde(default = "keep_days")]
    keep_days: u64,
    #[serde(default)]
    witness: Option<Storage>,
}

/// Config keys the platform owns: the catalog's routing manifest (what a
/// runner reads before it launches anything) and anything `_`-prefixed, such
/// as `_media`, the store's tile art. The platform delivers the whole version
/// config, so these reach the app too.
const PLATFORM_KEYS: &[&str] = &[
    "wasi",
    "threads",
    "set",
    "mem64",
    "gpuOptional",
    "cpuFallback",
    "volumes",
    "egress",
];

fn keep_days() -> u64 {
    30
}

fn main() -> String {
    "main".into()
}
fn object_mb() -> u64 {
    512
}

/// The guest's memory ceiling as the platform reports it (ENCLAVE_MEM_MB):
/// the defaults for the push limit and the cache are sized under it, so a
/// deployment given less memory refuses an oversized push instead of dying.
fn mem_mb() -> Option<u64> {
    std::env::var("ENCLAVE_MEM_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&m| m > 0)
}

pub struct Config {
    pub storage: Storage,
    pub master_key: String,
    pub users: Vec<(String, User, [u8; 32])>,
    pub public: Vec<String>,
    pub protected: Vec<String>,
    pub default_branch: String,
    pub max_push: u64,
    pub max_object: u64,
    pub cache_mb: usize,
    pub title: String,
    pub sso: Option<crate::sso::SsoConfig>,
    pub hooks: Vec<crate::hooks::Hook>,
    /// seconds a dropped ref value's history is kept before collection
    pub keep: u64,
    /// a second store that remembers the newest revision of every mutable
    /// document, so storage serving an older copy is caught after a restart
    pub witness: Option<Storage>,
}

fn secret(s: &str) -> Result<String, String> {
    match s.strip_prefix('$') {
        Some(n) => {
            let n = n.trim_start_matches('{').trim_end_matches('}');
            std::env::var(n).map_err(|_| format!("deployment secret {n} is not set"))
        }
        None => Ok(s.to_string()),
    }
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

pub fn valid_user(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
}

/// Repository names: one or two path segments of [A-Za-z0-9._-], no
/// leading dot, not ending in `.git` (the URL suffix is stripped).
pub fn valid_repo(s: &str) -> bool {
    let segs: Vec<&str> = s.split('/').collect();
    (1..=2).contains(&segs.len())
        && !s.ends_with(".git")
        && segs.iter().all(|g| {
            !g.is_empty()
                && g.len() <= 100
                && !g.starts_with('.')
                && g.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

fn check_storage(mut st: Storage, local_test: bool, what: &str) -> Result<Storage, String> {
    st.access_key = secret(&st.access_key)?;
    st.secret_key = secret(&st.secret_key)?;
    crate::client::parse_origin(&st.endpoint)?;
    if !st.endpoint.starts_with("https://") && !local_test {
        return Err(format!("{what}.endpoint must be https (local_test permits http)"));
    }
    if st.bucket.is_empty()
        || !st
            .bucket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(format!("{what}.bucket is invalid"));
    }
    if !st.prefix.is_empty()
        && (!st.prefix.ends_with('/') || st.prefix.starts_with('/') || st.prefix.contains(".."))
    {
        return Err(format!("{what}.prefix must be empty or like \"depot/\""));
    }
    if !local_test && (st.access_key.is_empty() || st.secret_key.is_empty()) {
        return Err(format!("{what} credentials are required"));
    }
    Ok(st)
}

impl Config {
    /// The platform delivers the config twice: the file (always, and the
    /// only channel past the env-size ceiling) and ENCLAVE_CONFIG. Both carry
    /// the text after `$NAME` secret substitution.
    pub fn load() -> Result<Config, String> {
        if let Ok(path) = std::env::var("ENCLAVE_CONFIG_FILE") {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                return Self::parse(&raw);
            }
        }
        let raw = std::env::var("ENCLAVE_CONFIG")
            .or_else(|_| std::env::var("DEPOT_CONFIG"))
            .map_err(|_| "set ENCLAVE_CONFIG")?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Config, String> {
        let mut v: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("config: {e}"))?;
        // keys the platform stamps into a version's config for its own use
        // (routing, store art) arrive with it; they are not ours to refuse
        if let Some(o) = v.as_object_mut() {
            o.retain(|k, _| !PLATFORM_KEYS.contains(&k.as_str()) && !k.starts_with('_'));
        }
        let r: Raw = serde_json::from_value(v).map_err(|e| format!("config: {e}"))?;
        let storage = check_storage(r.storage, r.local_test, "storage")?;
        let witness = match r.witness {
            Some(w) => {
                let w = check_storage(w, r.local_test, "witness")?;
                if w.endpoint == storage.endpoint
                    && w.bucket == storage.bucket
                    && w.access_key == storage.access_key
                {
                    return Err("witness must be a different store from storage (ideally another provider)".into());
                }
                Some(w)
            }
            None => None,
        };
        if r.keep_days > 3650 {
            return Err("keep_days 0..3650".into());
        }
        let master_key = secret(&r.master_key)?;
        if master_key.len() < 32 {
            return Err(
                "master_key must be at least 32 characters (use a deployment secret)".into(),
            );
        }
        let mut users = Vec::new();
        for (name, u) in r.users {
            if !valid_user(&name) {
                return Err(format!("bad user name {name:?}"));
            }
            let hash = match (&u.token, &u.token_sha256) {
                (None, None) if u.account.is_some() => [0u8; 32], // sign-in only
                // `token` must be a `$NAME` secret reference in the published
                // config; the platform substitutes the value before we see it,
                // so the text here cannot tell the two apart
                (Some(t), None) => {
                    let t = secret(t)?;
                    if t.len() < 24 {
                        return Err(format!("user {name}: token must be at least 24 characters"));
                    }
                    sha256(t.as_bytes())
                }
                (None, Some(h)) => {
                    let b = crate::git::unhex(h)
                        .filter(|b| b.len() == 32)
                        .ok_or(format!("user {name}: token_sha256 must be 64 hex"))?;
                    b.try_into().unwrap()
                }
                _ => {
                    return Err(format!(
                        "user {name}: give exactly one of token, token_sha256"
                    ))
                }
            };
            if let Some(a) = &u.account {
                if crate::sso::canonical_sub(a).as_deref() != Some(a.as_str()) {
                    return Err(format!("user {name}: account must be a canonical acct_… id or lowercase wallet address"));
                }
            }
            for p in u.read.iter().chain(&u.write) {
                if p.is_empty() || p.len() > 200 {
                    return Err(format!("user {name}: bad repository pattern"));
                }
            }
            users.push((name, u, hash));
        }
        for p in &r.protected {
            if !p.starts_with("refs/") {
                return Err(format!(
                    "protected patterns name full refs (refs/...), got {p}"
                ));
            }
        }
        if !crate::git::valid_refname(&format!("refs/heads/{}", r.default_branch)) {
            return Err("default_branch is not a valid branch name".into());
        }
        let mem = mem_mb();
        let cache_mb = r
            .cache_mb
            .unwrap_or_else(|| mem.map_or(256, |m| (m / 8).clamp(32, 256) as usize));
        let max_push_mb = r.max_push_mb.unwrap_or_else(|| {
            mem.map_or(1024, |m| {
                m.saturating_sub(cache_mb as u64 * 2 + 512).clamp(64, 1024)
            })
        });
        if !(1..=3072).contains(&max_push_mb)
            || !(1..=2048).contains(&r.max_object_mb)
            || !(16..=3072).contains(&cache_mb)
        {
            return Err("max_push_mb 1..3072, max_object_mb 1..2048, cache_mb 16..3072".into());
        }
        if let Some(m) = mem {
            if max_push_mb + cache_mb as u64 * 2 + 256 > m {
                eprintln!("[depot] warning: max_push_mb {max_push_mb} + cache_mb {cache_mb} may not fit the {m} MiB guest; a push that large can exhaust memory");
            }
        }
        let mut hooks = r.hooks;
        if hooks.len() > 16 {
            return Err("at most 16 hooks".into());
        }
        for h in &mut hooks {
            let ok =
                h.url.starts_with("https://") || (r.local_test && h.url.starts_with("http://"));
            if !ok || h.url.contains(['\r', '\n', ' ']) {
                return Err(format!("hook url must be https: {}", h.url));
            }
            h.secret = secret(&h.secret)?;
            if h.secret.len() < 16 {
                return Err("hook secrets must be at least 16 characters".into());
            }
        }
        let sso = match &r.sso {
            Some(s) => crate::sso::SsoConfig::from_config(&serde_json::json!({ "sso": s }))?,
            None => None,
        };
        Ok(Config {
            storage,
            master_key,
            users,
            public: r.public,
            protected: r.protected,
            default_branch: r.default_branch,
            max_push: max_push_mb << 20,
            max_object: r.max_object_mb << 20,
            cache_mb,
            title: r.title.unwrap_or_else(|| "depot".into()),
            sso,
            hooks,
            // DEPOT_KEEP_SECONDS overrides, for tests
            keep: std::env::var("DEPOT_KEEP_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(r.keep_days * 86_400),
            witness,
        })
    }

    /// The user a token belongs to (constant-time over every user).
    pub fn user_for(&self, token: &str) -> Option<&str> {
        use subtle::ConstantTimeEq;
        let h = sha256(token.as_bytes());
        let mut found = None;
        for (name, _, hash) in &self.users {
            if *hash != [0u8; 32] && bool::from(hash.ct_eq(&h)) {
                found = Some(name.as_str());
            }
        }
        found
    }

    /// The user an Enclave sign-in maps to.
    pub fn user_for_account(&self, sub: &str) -> Option<&str> {
        self.users
            .iter()
            .find(|(_, u, _)| u.account.as_deref() == Some(sub))
            .map(|(n, _, _)| n.as_str())
    }

    pub fn user(&self, name: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, u, _)| u)
    }

    pub fn is_protected(&self, refname: &str) -> bool {
        self.protected.iter().any(|p| glob(p, refname))
    }

    pub fn public_by_config(&self, repo: &str) -> bool {
        self.public.iter().any(|p| glob(p, repo))
    }
}

/// Who a request is: nobody, a configured user (by token or sign-in), or a
/// minted token carrying its own permissions.
#[derive(Clone, Debug, PartialEq)]
pub enum Who {
    Anonymous,
    User(String),
    Token(Box<crate::tokens::Token>),
}

impl Who {
    pub fn name(&self) -> &str {
        match self {
            Who::Anonymous => "anonymous",
            Who::User(n) => n,
            Who::Token(t) => &t.user,
        }
    }
}

pub fn can_read(cfg: &Config, who: &Who, repo: &str, public: bool) -> bool {
    if public || cfg.public_by_config(repo) {
        return true;
    }
    match who {
        Who::Anonymous => false,
        Who::User(n) => cfg
            .user(n)
            .is_some_and(|u| u.admin || u.read.iter().chain(&u.write).any(|p| glob(p, repo))),
        Who::Token(t) => t.admin || t.read.iter().chain(&t.write).any(|p| glob(p, repo)),
    }
}

pub fn can_write(cfg: &Config, who: &Who, repo: &str) -> bool {
    match who {
        Who::Anonymous => false,
        Who::User(n) => cfg
            .user(n)
            .is_some_and(|u| u.admin || u.write.iter().any(|p| glob(p, repo))),
        Who::Token(t) => t.admin || t.write.iter().any(|p| glob(p, repo)),
    }
}

pub fn is_admin(cfg: &Config, who: &Who) -> bool {
    match who {
        Who::User(n) => cfg.user(n).is_some_and(|u| u.admin),
        Who::Token(t) => t.admin,
        Who::Anonymous => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        std::env::set_var("T_DEPOT_TOKEN", "tok-steven-0123456789abcdefgh");
        Config::parse(&format!(
            r#"{{"storage":{{"endpoint":"http://127.0.0.1:9000","bucket":"b"}},"master_key":"0123456789abcdef0123456789abcdef",
            "local_test":true,
            "users":{{"steven":{{"token":"$T_DEPOT_TOKEN","admin":true}},
                      "ci":{{"token_sha256":"{}","read":["*"],"write":["enclave"]}}}},
            "public":["open/*"],"protected":["refs/heads/main"]}}"#,
            crate::git::hex(&sha256(b"ci-token-0123456789abcdefgh"))
        ))
        .unwrap()
    }

    #[test]
    fn tokens_and_acls() {
        let c = cfg();
        assert_eq!(c.user_for("tok-steven-0123456789abcdefgh"), Some("steven"));
        assert_eq!(c.user_for("ci-token-0123456789abcdefgh"), Some("ci"));
        assert_eq!(c.user_for("nope"), None);
        let ci = Who::User("ci".into());
        assert!(can_read(&c, &ci, "anything", false));
        assert!(can_write(&c, &ci, "enclave"));
        assert!(!can_write(&c, &ci, "enclave-apps"));
        assert!(can_read(&c, &Who::Anonymous, "open/x", false));
        assert!(!can_read(&c, &Who::Anonymous, "closed", false));
        assert!(can_read(&c, &Who::Anonymous, "closed", true));
        assert!(is_admin(&c, &Who::User("steven".into())));
        assert!(c.is_protected("refs/heads/main"));
        assert!(!c.is_protected("refs/heads/dev"));
    }

    #[test]
    fn refuses_bad_configs() {
        let base = r#""storage":{"endpoint":"https://x.r2.cloudflarestorage.com","bucket":"b","access_key":"a","secret_key":"s"}"#;
        assert!(Config::parse(&format!(r#"{{{base},"master_key":"short"}}"#)).is_err());
        // a substituted secret arrives as plain text
        assert!(Config::parse(&format!(
            r#"{{{base},"master_key":"0123456789abcdef0123456789abcdef","users":{{"u":{{"token":"substituted-token-value-xxxxx"}}}}}}"#
        ))
        .is_ok());
        assert!(Config::parse(&format!(
            r#"{{{base},"master_key":"0123456789abcdef0123456789abcdef","users":{{"u":{{"token":"short"}}}}}}"#
        ))
        .is_err());
        assert!(Config::parse(&format!(
            r#"{{{base},"master_key":"0123456789abcdef0123456789abcdef","bogus":1}}"#
        ))
        .is_err());
        assert!(Config::parse(&format!(
            r#"{{{base},"master_key":"0123456789abcdef0123456789abcdef"}}"#
        ))
        .is_ok());
    }

    #[test]
    fn shipped_template_parses() {
        for k in [
            "DEPOT_R2_ACCESS_KEY",
            "DEPOT_R2_SECRET_KEY",
            "DEPOT_ADMIN_TOKEN",
            "DEPOT_WITNESS_ACCESS_KEY",
            "DEPOT_WITNESS_SECRET_KEY",
        ] {
            std::env::set_var(k, "x".repeat(40));
        }
        std::env::set_var("DEPOT_MASTER_KEY", "m".repeat(64));
        let c = Config::parse(include_str!("../assets/deploy-config.template.json")).unwrap();
        assert!(c.is_protected("refs/tags/v1.2.3") && c.is_protected("refs/heads/main"));
        assert_eq!(c.storage.prefix, "depot/");
        assert!(c.witness.is_some() && c.keep == 30 * 86_400);
    }

    #[test]
    fn platform_keys_are_ignored_but_typos_are_not() {
        std::env::set_var("PK_K", "k".repeat(30));
        let base = format!(
            r#""storage":{{"endpoint":"https://a.example","bucket":"b","access_key":"$PK_K","secret_key":"$PK_K"}},"master_key":"{}""#,
            "m".repeat(40)
        );
        let with = format!(
            r#"{{{base},"wasi":"0.2","mem64":false,"_media":{{"thumbnail":"bafy","banner":"bafy2"}},"egress":"public-web"}}"#
        );
        assert!(Config::parse(&with).is_ok(), "platform keys ride along");
        let typo = format!(r#"{{{base},"protcted":["refs/heads/main"]}}"#);
        assert!(Config::parse(&typo).err().is_some_and(|e| e.contains("protcted")));
    }

    #[test]
    fn witness_must_be_another_store() {
        std::env::set_var("WT_K", "k".repeat(30));
        let cfg = |w: &str| {
            format!(
                r#"{{"storage":{{"endpoint":"https://a.example","bucket":"b","access_key":"$WT_K","secret_key":"$WT_K"}},
                   "master_key":"{}","witness":{w}}}"#,
                "m".repeat(40)
            )
        };
        let same = r#"{"endpoint":"https://a.example","bucket":"b","access_key":"$WT_K","secret_key":"$WT_K","prefix":"w/"}"#;
        assert!(Config::parse(&cfg(same)).is_err(), "same bucket, same key");
        let other = r#"{"endpoint":"https://s3.other.example","bucket":"w","access_key":"$WT_K","secret_key":"$WT_K"}"#;
        assert!(Config::parse(&cfg(other)).unwrap().witness.is_some());
        let plain = r#"{"endpoint":"http://s3.other.example","bucket":"w","access_key":"$WT_K","secret_key":"$WT_K"}"#;
        assert!(Config::parse(&cfg(plain)).is_err(), "https only");
    }

    #[test]
    fn repo_names() {
        for ok in ["enclave", "enclave-apps", "EnclaveHost/enclave", "a.b_c"] {
            assert!(valid_repo(ok), "{ok}");
        }
        for bad in [
            "", "a/b/c", ".hidden", "a/.b", "x.git", "a b", "a//b", "../x",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }
}
