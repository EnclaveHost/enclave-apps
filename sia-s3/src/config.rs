//! `ENCLAVE_CONFIG` (or `SIAS3_CONFIG` locally), JSON. String values of the
//! exact form `$NAME` / `${NAME}` resolve from the environment, which is how
//! deployment secrets arrive (the s3-ipfs-adapter convention).
//!
//! ```json
//! {
//!   "indexer": "https://sia.enclave.host",
//!   "appKey": "$SIA_APP_KEY",
//!   "region": "auto",
//!   "buckets": ["ipfs"],
//!   "credentials": [{ "accessKeyId": "$S3_ACCESS_KEY_ID", "secretAccessKey": "$S3_SECRET_ACCESS_KEY" }]
//! }
//! ```
//!
//! Replicated across independent indexers (replica.rs), `replicas` replaces
//! `indexer`; each replica's `appKey` defaults to the top-level one. It may also
//! be one `$NAME` string holding that JSON array, so the owner can change the
//! set with a secrets update instead of a new catalog version:
//!
//! ```json
//! { "replicas": [ { "name": "minipc", "indexer": "https://sia.enclave.host", "appKey": "$SIA_APP_KEY" },
//!                 { "name": "b", "indexer": "https://indexd.example.org", "appKey": "$SIA_APP_KEY_B" } ],
//!   "writeQuorum": 1, ... }
//! ```
//!
//! Indexer URLs are canonicalized (scheme, lowercase host without a trailing
//! dot, port only when not the scheme's default, path without a trailing
//! slash) and must be distinct. That only stops one endpoint being counted as
//! two quorum votes by accident: distinct URLs do not show distinct operators
//! (one operator can run many names), which the deployer has to establish.

use std::time::Duration;

use serde_json::Value;

use crate::replica::Timing;
use crate::store::Opts;

/// One indexer the data is replicated to.
#[derive(Clone, PartialEq)]
pub struct ReplicaCfg {
    pub name: String,
    /// canonical form (see the module docs)
    pub indexer: String,
    /// 32-byte seed of the app key registered on this indexer
    pub app_key: [u8; 32],
}

/// Never prints the seed.
impl std::fmt::Debug for ReplicaCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicaCfg").field("name", &self.name).field("indexer", &self.indexer).field("app_key", &"<redacted>").finish()
    }
}

pub struct Config {
    pub replicas: Vec<ReplicaCfg>,
    /// replicas that must commit a write (default: a majority)
    pub write_quorum: usize,
    pub region: String,
    pub buckets: Vec<String>,
    /// (access key id, secret)
    pub credentials: Vec<(String, String)>,
    /// largest single PUT / UploadPart body accepted
    pub max_body: usize,
    /// objects below this size are written alone, then repacked by the compactor
    pub compact_max_object: u64,
    pub compact_every_secs: u64,
    pub repair_every_secs: u64,
    /// bytes one repair pass copies at most
    pub repair_max_bytes: u64,
    pub opts: Opts,
    pub timing: Timing,
}

fn key32(hex_key: &str) -> Result<[u8; 32], String> {
    let key = hex::decode(hex_key.trim()).map_err(|_| "appKey is not hex")?;
    key.try_into().map_err(|_| "appKey must be 32 bytes".to_string())
}

/// The canonical form of an indexer URL (module docs). Credentials, a query or a fragment are refused: the SDK
/// appends API paths to this URL, and a secret in it would be logged and shown on the status page.
pub fn canonical_indexer(raw: &str) -> Result<String, String> {
    let u = url::Url::parse(raw.trim()).map_err(|e| format!("not a URL ({e})"))?;
    let default_port = match u.scheme() {
        "https" => 443,
        "http" => 80,
        s => return Err(format!("scheme {s} is not http or https")),
    };
    if !u.username().is_empty() || u.password().is_some() {
        return Err("credentials do not belong in the URL".into());
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err("a query or fragment is not allowed".into());
    }
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        Some(url::Host::Ipv4(a)) => a.to_string(),
        Some(url::Host::Ipv6(a)) => format!("[{a}]"),
        None => return Err("no host".into()),
    };
    if host.is_empty() {
        return Err("no host".into());
    }
    let mut out = format!("{}://{host}", u.scheme());
    match u.port_or_known_default() {
        Some(p) if p != default_port => out.push_str(&format!(":{p}")),
        _ => {}
    }
    out.push_str(u.path().trim_end_matches('/'));
    Ok(out)
}

/// `replicas` (already resolved if it was a `$NAME` string), else the single top-level `indexer`.
fn replicas(j: &Value, default_key: Option<&str>) -> Result<Vec<ReplicaCfg>, String> {
    let list = match &j["replicas"] {
        Value::Null => {
            let indexer = resolve(&j["indexer"]).unwrap_or_else(|| "https://sia.enclave.host".into());
            let indexer = canonical_indexer(&indexer).map_err(|e| format!("indexer: {e}"))?;
            let key = default_key.ok_or("appKey is missing or unresolved")?;
            return Ok(vec![ReplicaCfg { name: "primary".into(), indexer, app_key: key32(key)? }]);
        }
        v @ Value::String(_) => {
            let raw = resolve(v).ok_or("replicas is unresolved")?;
            serde_json::from_str::<Value>(&raw).map_err(|e| format!("replicas is not a JSON array: {e}"))?
        }
        v => v.clone(),
    };
    let arr = list.as_array().filter(|a| !a.is_empty()).ok_or("replicas must be a non-empty array")?;
    if arr.len() > 16 {
        return Err("at most 16 replicas".into());
    }
    let mut out: Vec<ReplicaCfg> = Vec::new();
    for (i, r) in arr.iter().enumerate() {
        let name = r["name"].as_str().map(String::from).unwrap_or_else(|| format!("r{i}"));
        if name.is_empty() || name.len() > 32 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err(format!("bad replica name {name:?}"));
        }
        let indexer = resolve(&r["indexer"]).ok_or_else(|| format!("replica {name}: indexer is missing or unresolved"))?;
        let indexer = canonical_indexer(&indexer).map_err(|e| format!("replica {name}: indexer: {e}"))?;
        // a replica without a key of its own uses the top-level one; one whose `$NAME` is unset is an error,
        // never a silent fallback to another indexer's key
        let key = match &r["appKey"] {
            Value::Null => default_key.map(String::from),
            v => resolve(v),
        }
        .ok_or_else(|| format!("replica {name}: appKey is missing or unresolved"))?;
        let rc = ReplicaCfg { name, indexer, app_key: key32(&key).map_err(|e| format!("replica {}: {e}", r["name"]))? };
        if out.iter().any(|o| o.name == rc.name) {
            return Err(format!("duplicate replica name {:?}", rc.name));
        }
        if out.iter().any(|o| o.indexer == rc.indexer) {
            return Err(format!("replica {}: indexer {} is listed twice", rc.name, rc.indexer));
        }
        out.push(rc);
    }
    for r in &out {
        if r.indexer.starts_with("http://") {
            // kept for a local or tunnelled indexer: the SDK signs requests with the app key and verifies sealed
            // objects, but over plain HTTP anyone on the path sees which objects exist and can withhold or replay
            // the indexer's answers
            eprintln!("[sia-s3] config: replica {} uses plain HTTP; only for an indexer on a trusted path", r.name);
        }
    }
    Ok(out)
}

fn resolve(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    let name = s
        .strip_prefix("${")
        .and_then(|r| r.strip_suffix('}'))
        .or_else(|| s.strip_prefix('$'));
    match name {
        Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            match std::env::var(n) {
                Ok(val) if !val.is_empty() => {
                    eprintln!("[sia-s3] config: resolved ${n} from the environment");
                    Some(val)
                }
                _ => {
                    eprintln!("[sia-s3] config: ${n} is not set in the environment");
                    None
                }
            }
        }
        _ => Some(s.to_string()),
    }
}

pub fn load() -> Result<Config, String> {
    let raw = std::env::var("ENCLAVE_CONFIG")
        .or_else(|_| std::env::var("SIAS3_CONFIG"))
        .map_err(|_| "ENCLAVE_CONFIG is not set".to_string())?;
    parse(&raw)
}

pub fn parse(raw: &str) -> Result<Config, String> {
    let j: Value = serde_json::from_str(raw).map_err(|e| format!("config is not JSON: {e}"))?;

    let default_key = if j["appKey"].is_null() { None } else { resolve(&j["appKey"]) };
    let replicas = replicas(&j, default_key.as_deref())?;
    let write_quorum = match &j["writeQuorum"] {
        Value::Null => replicas.len() / 2 + 1,
        v => v.as_u64().ok_or("writeQuorum must be a number")? as usize,
    };
    if !(1..=replicas.len()).contains(&write_quorum) {
        return Err(format!("writeQuorum must be 1-{} (the number of replicas)", replicas.len()));
    }
    let secs = |name: &str, default: u64, min: u64| -> Result<u64, String> {
        match j[name].as_u64() {
            None if j[name].is_null() => Ok(default),
            Some(v) if v >= min => Ok(v),
            _ => Err(format!("{name} must be a number of seconds, at least {min}")),
        }
    };
    let meta_timeout = secs("metadataTimeoutSecs", 120, 5)?;

    let buckets: Vec<String> = j["buckets"]
        .as_array()
        .map(|a| a.iter().filter_map(|b| b.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if buckets.is_empty() {
        return Err("buckets must name at least one bucket".into());
    }
    for b in &buckets {
        if b.is_empty() || b.len() > 63 || !b.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.') {
            return Err(format!("bad bucket name {b:?}"));
        }
    }

    let mut credentials = Vec::new();
    for c in j["credentials"].as_array().into_iter().flatten() {
        if let (Some(id), Some(secret)) = (resolve(&c["accessKeyId"]), resolve(&c["secretAccessKey"])) {
            credentials.push((id, secret));
        }
    }
    if credentials.is_empty() {
        return Err("no usable credentials (every request must be signed)".into());
    }

    Ok(Config {
        replicas,
        write_quorum,
        region: resolve(&j["region"]).unwrap_or_else(|| "auto".into()),
        buckets,
        credentials,
        max_body: j["maxBodyBytes"].as_u64().unwrap_or(64 << 20) as usize,
        compact_max_object: j["compactMaxObjectBytes"].as_u64().unwrap_or(8 << 20),
        compact_every_secs: j["compactEverySecs"].as_u64().unwrap_or(1800),
        repair_every_secs: j["repairEverySecs"].as_u64().unwrap_or(300).max(10),
        repair_max_bytes: j["repairMaxBytesPerPass"].as_u64().unwrap_or(1 << 30),
        opts: Opts {
            shards: (10, 20),
            meta_timeout: Duration::from_secs(meta_timeout),
            upload_timeout: Duration::from_secs(secs("uploadTimeoutSecs", 1800, 60)?),
        },
        timing: Timing {
            upload_grace: Duration::from_secs(secs("uploadGraceSecs", 30, 0)?),
            meta_grace: Duration::from_secs(secs("metadataGraceSecs", 5, 0)?),
            stall: Duration::from_secs(secs("stallTimeoutSecs", 120, 10)?),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const K2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn cfg(extra: &str) -> Result<Config, String> {
        parse(&format!(r#"{{"buckets":["ipfs"],"credentials":[{{"accessKeyId":"a","secretAccessKey":"s"}}]{extra}}}"#))
    }

    #[test]
    fn a_0_1_config_is_one_replica() {
        let c = cfg(&format!(r#","indexer":"https://sia.enclave.host","appKey":"{K1}""#)).unwrap();
        assert_eq!(c.replicas, vec![ReplicaCfg { name: "primary".into(), indexer: "https://sia.enclave.host".into(), app_key: [0x11; 32] }]);
        assert_eq!((c.opts.meta_timeout.as_secs(), c.opts.upload_timeout.as_secs(), c.timing.meta_grace.as_secs()), (120, 1800, 5));
        assert_eq!(c.write_quorum, 1);
    }

    #[test]
    fn replicas_default_to_a_majority_and_the_top_level_key() {
        let c = cfg(&format!(
            r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"https://a.example"}},{{"name":"b","indexer":"https://b.example","appKey":"{K2}"}},{{"indexer":"http://c.example:9980"}}]"#
        ))
        .unwrap();
        assert_eq!(c.replicas.len(), 3);
        assert_eq!(c.write_quorum, 2);
        assert_eq!((c.replicas[0].app_key, c.replicas[1].app_key), ([0x11; 32], [0x22; 32]));
        assert_eq!(c.replicas[2].name, "r2");
    }

    #[test]
    fn replicas_may_arrive_as_one_json_string() {
        let list = format!(r#"[{{"name":"a","indexer":"https://a.example","appKey":"{K1}"}},{{"name":"b","indexer":"https://b.example","appKey":"{K2}"}}]"#);
        let c = cfg(&format!(r#","replicas":{},"writeQuorum":1"#, serde_json::to_string(&list).unwrap())).unwrap();
        assert_eq!((c.replicas.len(), c.write_quorum), (2, 1));
    }

    #[test]
    fn unsafe_replica_sets_are_refused() {
        let two = format!(r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"https://a.example"}},{{"name":"b","indexer":"https://b.example"}}]"#);
        for bad in [
            // the same indexer twice would count one operator as two quorum votes
            format!(r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"https://a.example"}},{{"name":"b","indexer":"https://A.example/"}}]"#),
            format!(r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"https://a.example"}},{{"name":"a","indexer":"https://b.example"}}]"#),
            format!(r#"{two},"writeQuorum":0"#),
            format!(r#"{two},"writeQuorum":3"#),
            format!(r#","replicas":[{{"name":"a","indexer":"https://a.example"}}]"#),
            format!(r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"https://a.example","appKey":"$SIAS3_TEST_UNSET_KEY"}}]"#),
            format!(r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"ftp://a.example"}}]"#),
            format!(r#","appKey":"{K1}","replicas":[]"#),
            format!(r#","appKey":"{K1}","metadataTimeoutSecs":1"#),
        ] {
            assert!(cfg(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn indexer_urls_are_canonical_and_aliases_count_once() {
        for (raw, want) in [
            ("https://Sia.Enclave.Host", "https://sia.enclave.host"),
            ("https://sia.enclave.host.:443/", "https://sia.enclave.host"),
            ("http://127.0.0.1:80", "http://127.0.0.1"),
            ("http://127.1:9980/", "http://127.0.0.1:9980"),
            ("https://[::1]:8443/indexd/", "https://[::1]:8443/indexd"),
        ] {
            assert_eq!(canonical_indexer(raw).unwrap(), want, "{raw}");
        }
        for bad in ["https://user:pw@a.example", "https://a.example/?k=v", "https://a.example/#x", "ftp://a.example", "a.example", "https://"] {
            assert!(canonical_indexer(bad).is_err(), "{bad}");
        }
        for alias in ["https://A.example.:443", "https://a.example/", "HTTPS://a.example"] {
            let two = format!(r#","appKey":"{K1}","replicas":[{{"name":"a","indexer":"https://a.example"}},{{"name":"b","indexer":"{alias}"}}]"#);
            assert!(cfg(&two).is_err(), "{alias} counted as a second indexer");
        }
    }

    #[test]
    fn replica_debug_output_never_shows_the_seed() {
        let c = cfg(&format!(r#","indexer":"https://sia.enclave.host","appKey":"{K1}""#)).unwrap();
        let shown = format!("{:?}", c.replicas);
        assert!(shown.contains("redacted") && !shown.contains("17") && !shown.contains("11, 17"), "{shown}");
    }
}
