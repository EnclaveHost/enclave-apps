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

use serde_json::Value;

pub struct Config {
    pub indexer: String,
    /// 32-byte seed of the app key registered on the indexer (hex)
    pub app_key: [u8; 32],
    pub region: String,
    pub buckets: Vec<String>,
    /// (access key id, secret)
    pub credentials: Vec<(String, String)>,
    /// largest single PUT / UploadPart body accepted
    pub max_body: usize,
    /// objects below this size are written alone, then repacked by the compactor
    pub compact_max_object: u64,
    pub compact_every_secs: u64,
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
    let j: Value = serde_json::from_str(&raw).map_err(|e| format!("config is not JSON: {e}"))?;

    let indexer = resolve(&j["indexer"]).unwrap_or_else(|| "https://sia.enclave.host".into());
    let key_hex = resolve(&j["appKey"]).ok_or("appKey is missing or unresolved")?;
    let key = hex::decode(key_hex.trim()).map_err(|_| "appKey is not hex")?;
    let app_key: [u8; 32] = key.try_into().map_err(|_| "appKey must be 32 bytes")?;

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
        indexer,
        app_key,
        region: resolve(&j["region"]).unwrap_or_else(|| "auto".into()),
        buckets,
        credentials,
        max_body: j["maxBodyBytes"].as_u64().unwrap_or(64 << 20) as usize,
        compact_max_object: j["compactMaxObjectBytes"].as_u64().unwrap_or(8 << 20),
        compact_every_secs: j["compactEverySecs"].as_u64().unwrap_or(1800),
    })
}
