//! Bulk import: `sia-s3 import <bucket> <manifest.json>`
//!
//! The manifest is `[{"key", "size", "url"}]`. Each object is fetched from its
//! URL in ranged chunks (memory stays bounded for multi-GiB files) and streamed
//! into a packed upload, so many small objects share slabs instead of each
//! paying for a full one. Objects are pinned per batch with the same metadata
//! the server writes, so a running sia-s3 sees them after its next index load.
//! Keys already present with the same size are skipped: re-running resumes.

use std::time::Instant;

use md5::{Digest, Md5};
use serde::Deserialize;
use sia_storage::{PackedUploadOptions, Sdk};
use tokio::io::AsyncWriteExt;

use crate::store::{Entry, Meta, Store};

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

pub async fn run(sdk: Sdk, bucket: String, manifest: String) -> Result<(), String> {
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
        let now = chrono::Utc::now().timestamp();
        for ((item, etag), mut obj) in batch.iter().zip(etags).zip(objs) {
            let meta = Meta {
                v: 1,
                p: true,
                e: vec![Entry { b: bucket.clone(), k: item.key.clone(), s: item.size, t: etag, m: now, c: None, x: vec![] }],
                mp: None,
            };
            obj.metadata = serde_json::to_vec(&meta).map_err(|e| e.to_string())?;
            store.sdk.pin_object(&obj).await.map_err(|e| format!("{}: pin: {e}", item.key))?;
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
