//! Bulk import: `sia-s3 import <bucket> <manifest.json>`
//!
//! The manifest is `[{"key", "size", "url"}]`. Each object is fetched from its
//! URL in ranged chunks (memory stays bounded for multi-GiB files) and streamed
//! into a packed upload, so many small objects share slabs instead of each
//! paying for a full one. Objects are pinned per batch with the same metadata
//! the server writes, so a running sia-s3 sees them after its next index load.
//! Keys already present with the same size are skipped: re-running resumes.
//!
//! With several indexers configured, run it once per replica (`sia-s3 import
//! <bucket> <manifest> <replica>`) with the same `SIAS3_IMPORT_VERSION_MS`: each
//! key's version is then identical on every replica (clock from that variable,
//! write id from the key and its content), so repair has nothing to copy.

use std::time::Instant;

use md5::{Digest, Md5};
use serde::Deserialize;
use sha2::Sha256;
use sia_storage::{PackedUploadOptions, Sdk};
use tokio::io::AsyncWriteExt;

use crate::store::{Entry, Store};

#[derive(Deserialize, Clone)]
struct Item {
    key: String,
    size: u64,
    url: String,
}

const CHUNK: u64 = 32 << 20;
const BATCH_BYTES: u64 = 512 << 20;

async fn fetch(client: &reqwest_wasi::Client, item: &Item, mut w: tokio::io::DuplexStream) -> Result<String, String> {
    let mut md5 = Md5::new();
    let mut off = 0u64;
    while off < item.size {
        let end = (off + CHUNK).min(item.size) - 1;
        let mut tries = 0;
        let body = loop {
            let r = client
                .get(item.url.as_str())
                .header("range", format!("bytes={off}-{end}"))
                .timeout(std::time::Duration::from_secs(300))
                .send()
                .await;
            match r {
                Ok(resp) if resp.status().as_u16() == 206 || (resp.status().as_u16() == 200 && off == 0 && end + 1 == item.size) => {
                    let b = resp.bytes().await.map_err(|e| e.to_string())?;
                    if b.len() as u64 == end - off + 1 {
                        break b;
                    }
                }
                Ok(resp) => eprintln!("[import] {}: HTTP {} for bytes {off}-{end}", item.key, resp.status()),
                Err(e) => eprintln!("[import] {}: {e}", item.key),
            }
            tries += 1;
            if tries >= 5 {
                return Err(format!("{}: giving up on bytes {off}-{end}", item.key));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2 * tries)).await;
        };
        md5.update(&body);
        w.write_all(&body).await.map_err(|e| e.to_string())?;
        off = end + 1;
    }
    w.shutdown().await.map_err(|e| e.to_string())?;
    Ok(hex::encode(md5.finalize()))
}

/// The write id of an imported key: the same key with the same content gets the same id on every replica.
fn import_wid(bucket: &str, key: &str, etag: &str, size: u64) -> String {
    let mut h = Sha256::new();
    for part in [bucket.as_bytes(), key.as_bytes(), etag.as_bytes(), &size.to_be_bytes()] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    hex::encode(&h.finalize()[..8])
}

pub async fn run(sdk: Sdk, bucket: String, manifest: String) -> Result<(), String> {
    let version_ms: u64 = match std::env::var("SIAS3_IMPORT_VERSION_MS") {
        Ok(v) => v.parse().map_err(|_| "SIAS3_IMPORT_VERSION_MS must be milliseconds since 1970")?,
        Err(_) => chrono::Utc::now().timestamp_millis() as u64,
    };
    eprintln!("[import] version clock {version_ms} (set SIAS3_IMPORT_VERSION_MS to this for the other replicas)");
    let items: Vec<Item> = serde_json::from_slice(&std::fs::read(&manifest).map_err(|e| format!("{manifest}: {e}"))?)
        .map_err(|e| format!("manifest: {e}"))?;
    let store = Store::load(sdk, 0).await?;
    let todo: Vec<Item> = {
        let idx = store.idx.lock().await;
        items
            .into_iter()
            .filter(|i| i.size > 0) // empty objects go through the normal PUT path
            .filter(|i| idx.keys.get(&(bucket.clone(), i.key.clone())).is_none_or(|k| k.entry.s != i.size))
            .collect()
    };
    let total: u64 = todo.iter().map(|i| i.size).sum();
    eprintln!("[import] {} objects ({:.2} GiB) to import into {bucket}", todo.len(), total as f64 / (1u64 << 30) as f64);
    let client = reqwest_wasi::Client::new();
    let verbose = std::env::var("IMPORT_VERBOSE").is_ok();
    let started = Instant::now();
    let mut done_bytes = 0u64;
    let mut done = 0usize;

    let mut i = 0;
    while i < todo.len() {
        // one batch: up to BATCH_BYTES (a single larger object is its own batch)
        let mut batch = Vec::new();
        let mut bytes = 0u64;
        while i < todo.len() && (batch.is_empty() || bytes + todo[i].size <= BATCH_BYTES) {
            bytes += todo[i].size;
            batch.push(todo[i].clone());
            i += 1;
        }
        let mut pu = store
            .sdk
            .upload_packed(PackedUploadOptions { max_buffered_slabs: Some(1), ..Default::default() })
            .map_err(|e| e.to_string())?;
        let mut etags = Vec::with_capacity(batch.len());
        for item in &batch {
            let (tx, rx) = tokio::io::duplex(4 << 20);
            let fetcher = fetch(&client, item, tx);
            let adder = pu.add(rx);
            let t0 = Instant::now();
            let (etag, added) = tokio::join!(fetcher, adder);
            let etag = etag?;
            if verbose {
                eprintln!("[import]   {} ({} B) fetched+packed in {:?}", item.key, item.size, t0.elapsed());
            }
            let n = added.map_err(|e| format!("{}: {e}", item.key))?;
            if n != item.size {
                return Err(format!("{}: packed {n} bytes, expected {}", item.key, item.size));
            }
            etags.push(etag);
        }
        if verbose {
            eprintln!("[import]   finalizing batch of {} objects ({} B)", batch.len(), bytes);
        }
        let objs = pu.finalize().await.map_err(|e| e.to_string())?;
        if objs.len() != batch.len() {
            return Err(format!("packed upload returned {} objects for {}", objs.len(), batch.len()));
        }
        let m = (version_ms / 1000) as i64;
        for ((item, etag), obj) in batch.iter().zip(etags).zip(objs) {
            let i = import_wid(&bucket, &item.key, &etag, item.size);
            let entry = Entry { b: bucket.clone(), k: item.key.clone(), s: item.size, t: etag, m, c: None, x: vec![], h: version_ms, i };
            // the same placement rules as the server: inline, or a manifest for a key too long for metadata
            let placement = store.stage_put(&obj, &entry).await.map_err(|e| format!("{}: {e}", item.key))?;
            store.commit_put(obj, placement, entry, true).await.map_err(|e| format!("{}: pin: {e}", item.key))?;
            done += 1;
            done_bytes += item.size;
        }
        let secs = started.elapsed().as_secs_f64().max(1.0);
        eprintln!(
            "[import] {done}/{} objects, {:.2}/{:.2} GiB, {:.1} MiB/s",
            todo.len(),
            done_bytes as f64 / (1u64 << 30) as f64,
            total as f64 / (1u64 << 30) as f64,
            done_bytes as f64 / (1u64 << 20) as f64 / secs
        );
    }
    eprintln!("[import] done: {done} objects in {:.0}s", started.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::import_wid;

    #[test]
    fn import_write_ids_depend_only_on_the_key_and_its_content() {
        let a = import_wid("ipfs", "pins/x", "abc", 3);
        assert_eq!(a, import_wid("ipfs", "pins/x", "abc", 3));
        assert_eq!(a.len(), 16);
        for other in [import_wid("ipfs", "pins/y", "abc", 3), import_wid("ipfs", "pins/x", "abd", 3), import_wid("ipfs", "pins/x", "abc", 4), import_wid("ipfspins/", "x", "abc", 3)] {
            assert_ne!(a, other);
        }
    }
}
