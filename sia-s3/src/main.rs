//! sia-s3: an S3 endpoint backed by Sia, for enclave.host.
//!
//! Data is encrypted and erasure-coded client-side (inside this enclave) by the
//! Sia SDK and stored on Sia hosts through an indexer (default
//! https://sia.enclave.host). The S3 key index lives in the objects' own sealed
//! metadata (see store.rs), so the app keeps no local state: a restart reloads
//! the index from the indexer's object feed.
//!
//! The one platform rule: read `ENCLAVE_PORTS` and bind the actual port.

mod config;
mod recovery;
mod diag;
mod http;
mod import;
mod s3;
mod sigv4;
mod store;

use std::rc::Rc;
use std::time::Duration;

use sia_storage::{AppKey, AppMetadata, Builder, app_id};
use tokio::net::TcpListener;

use crate::http::{ReadError, read_request, write_head};
use crate::s3::App;
use crate::store::Store;

// Must match the metadata registered for this app key on the indexer.
const META: AppMetadata = AppMetadata {
    id: app_id!("5e1a0c3e7f2d4b19a8c6e0d4f3b2a1908e7d6c5b4a3f2e1d0c9b8a7f6e5d4c3b"),
    name: "sia-s3",
    description: "S3 over Sia for enclave.host platform storage",
    service_url: "https://enclave.host",
    logo_url: None,
    callback_url: None,
};


/// stderr logger for the SDK's `log` output: warnings and errors always,
/// everything from sia_* crates at debug when SIAS3_LOG=debug.
struct Logger { debug: bool }
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn || (self.debug && m.target().starts_with("sia"))
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {} {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

/// `ENCLAVE_PORTS=http:8000=18321,...` → the actual port for `http:8000`.
fn port() -> u16 {
    let default = 8000;
    let Ok(ports) = std::env::var("ENCLAVE_PORTS") else { return default };
    for spec in ports.split(',') {
        let mut it = spec.split(['=', ':']);
        if let (Some("http"), Some("8000"), Some(actual)) = (it.next(), it.next(), it.next()) {
            if let Ok(p) = actual.trim().parse() {
                return p;
            }
        }
    }
    default
}

async fn boot(app: Rc<App>, cfg_indexer: String, key: [u8; 32], compact_max: u64, compact_every: u64) {
    let mut wait = 5u64;
    let store = loop {
        let attempt = async {
            let sdk = Builder::new(cfg_indexer.as_str(), META)
                .map_err(|e| e.to_string())?
                .connected(&AppKey::import(key))
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "the app key is not authorized on the indexer".to_string())?;
            Store::load(sdk, compact_max).await
        };
        match attempt.await {
            Ok(s) => break Rc::new(s),
            Err(e) => {
                eprintln!("[sia-s3] boot: {e}; retrying in {wait}s");
                *app.boot_error.borrow_mut() = Some(e);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                wait = (wait * 2).min(300);
            }
        }
    };
    *app.boot_error.borrow_mut() = None;
    *app.store.borrow_mut() = Some(store.clone());
    eprintln!("[sia-s3] ready");
    store.sweep().await;

    loop {
        tokio::time::sleep(Duration::from_secs(compact_every)).await;
        match store.compact().await {
            Ok(n) => {
                let mut st = store.stats.borrow_mut();
                st.compactions += 1;
                st.compacted_objects += n as u64;
                st.last_compaction = Some(chrono::Utc::now().to_rfc3339());
                if n > 0 {
                    eprintln!("[sia-s3] compactor: repacked {n} objects");
                }
            }
            Err(e) => {
                eprintln!("[sia-s3] compactor: {e}");
                store.stats.borrow_mut().last_error = Some(e);
            }
        }
    }
}

async fn conn(app: Rc<App>, mut s: tokio::net::TcpStream, max_body: usize) {
    let mut carry = Vec::new();
    loop {
        let req = match read_request(&mut s, &mut carry, max_body).await {
            Ok(r) => r,
            Err(ReadError::Closed) => return,
            Err(ReadError::TooLarge) => {
                let _ = write_head(&mut s, 413, &[], 0, false).await;
                return;
            }
            Err(ReadError::Bad(e)) => {
                let body = format!("<?xml version=\"1.0\"?><Error><Code>BadRequest</Code><Message>{e}</Message></Error>");
                let _ = write_head(&mut s, 400, &[("Content-Type".into(), "application/xml".into())], body.len() as u64, false).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut s, body.as_bytes()).await;
                return;
            }
        };
        if s3::handle(&app, &req, &mut s).await.is_err() || !req.keep_alive {
            return;
        }
    }
}

fn main() {
    let debug = std::env::var("SIAS3_LOG").is_ok_and(|v| v == "debug");
    if log::set_boxed_logger(Box::new(Logger { debug })).is_ok() {
        log::set_max_level(if debug { log::LevelFilter::Debug } else { log::LevelFilter::Warn });
    }
    let recovery_args: Vec<String> = std::env::args().collect();
    if recovery_args.get(1).map(String::as_str) == Some("recovery") {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let local = tokio::task::LocalSet::new();
        if let Err(e) = local.block_on(&rt, recovery::command(&recovery_args[2..])) {
            eprintln!("[recovery] {e}");
            std::process::exit(1);
        }
        return;
    }
    let cfg = match config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sia-s3] config: {e}");
            std::process::exit(2);
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let local = tokio::task::LocalSet::new();

    // `sia-s3 import <bucket> <manifest.json>`: bulk-load objects, then exit
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("import") {
        let (Some(bucket), Some(manifest)) = (args.get(2).cloned(), args.get(3).cloned()) else {
            eprintln!("usage: sia-s3 import <bucket> <manifest.json>");
            std::process::exit(2);
        };
        let res = local.block_on(&rt, async move {
            let sdk = Builder::new(cfg.indexer.as_str(), META)
                .map_err(|e| e.to_string())?
                .connected(&AppKey::import(cfg.app_key))
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the app key is not authorized on the indexer")?;
            import::run(sdk, bucket, manifest).await
        });
        if let Err(e) = res {
            eprintln!("[import] FAILED: {e}");
            std::process::exit(1);
        }
        return;
    }

    local.block_on(&rt, async move {
        let app = Rc::new(App {
            store: Default::default(),
            buckets: cfg.buckets.clone(),
            credentials: cfg.credentials.clone(),
            region: cfg.region.clone(),
            indexer: cfg.indexer.clone(),
            boot_error: Default::default(),
        });
        let p = port();
        let listener = match TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[sia-s3] bind 127.0.0.1:{p}: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("[sia-s3] listening on 127.0.0.1:{p}; indexer {}; buckets {:?}", cfg.indexer, cfg.buckets);
        tokio::task::spawn_local(boot(app.clone(), cfg.indexer.clone(), cfg.app_key, cfg.compact_max_object, cfg.compact_every_secs));
        loop {
            match listener.accept().await {
                Ok((s, _)) => {
                    tokio::task::spawn_local(conn(app.clone(), s, cfg.max_body));
                }
                Err(e) => {
                    eprintln!("[sia-s3] accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}
