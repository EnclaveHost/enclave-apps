//! wasip2 spike: can sia_storage run as an enclave app?
//!   sia-spike connect <keyfile>   request approval, wait, register, save the app key
//!   sia-spike test <keyfile>      upload, pin, ranged download, list, delete
use std::io::Cursor;
use std::time::Instant;

use sia_storage::{AppKey, AppMetadata, Builder, DownloadOptions, Object, UploadOptions, app_id};
use tokio::io::AsyncReadExt;

const INDEXER: &str = "https://sia.storage";
const META: AppMetadata = AppMetadata {
    id: app_id!("5e1a0c3e7f2d4b19a8c6e0d4f3b2a1908e7d6c5b4a3f2e1d0c9b8a7f6e5d4c3b"),
    name: "enclave sia-s3 (spike)",
    description: "wasip2 feasibility test for an S3-on-Sia enclave app",
    service_url: "https://enclave.host",
    logo_url: None,
    callback_url: None,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let res = rt.block_on(async {
        match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("connect"), Some(kf)) => connect(kf).await,
            (Some("test"), Some(kf)) => test(kf).await,
            (Some("settings"), Some(hf)) => settings(hf).await,
            _ => Err("usage: sia-spike connect|test <keyfile>".into()),
        }
    });
    if let Err(e) = res {
        eprintln!("FAIL: {e}");
        std::process::exit(1);
    }
}

type R<T> = Result<T, Box<dyn std::error::Error>>;

async fn connect(keyfile: &str) -> R<()> {
    let t = Instant::now();
    let b = Builder::new(INDEXER, META)?.request_connection().await?;
    println!("indexer reachable over TLS in {:?}", t.elapsed());
    println!("APPROVE: {}", b.response_url());
    let b = b.wait_for_approval().await?;
    let phrase = std::env::var("SIA_PHRASE").unwrap_or_else(|_| sia_storage::generate_recovery_phrase());
    let sdk = b.register(&phrase).await?;
    std::fs::write(keyfile, hex(&sdk.app_key().export()))?;
    std::fs::write(format!("{keyfile}.phrase"), phrase)?;
    println!("registered; app key saved");
    Ok(())
}

async fn test(keyfile: &str) -> R<()> {
    let key: [u8; 32] = unhex(std::fs::read_to_string(keyfile)?.trim())?;
    let app_key = AppKey::import(key);
    let t = Instant::now();
    let sdk = Builder::new(INDEXER, META)?
        .connected(&app_key)
        .await?
        .ok_or("app key not authorized")?;
    println!("connected in {:?}; account: {:?}", t.elapsed(), sdk.account().await.map(|a| format!("{a:?}").chars().take(200).collect::<String>()));

    let n: usize = std::env::var("SPIKE_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(20 << 20);
    let mut data = vec![0u8; n];
    getrandom(&mut data);

    let t = Instant::now();
    let mut obj = sdk.upload(Object::default(), Cursor::new(data.clone()), UploadOptions::default()).await?;
    let up = t.elapsed();
    obj.metadata = b"{\"key\":\"spike/random.bin\"}".to_vec();
    sdk.pin_object(&obj).await?;
    println!("uploaded+pinned {n} bytes in {up:?} ({:.2} MiB/s); id {}", n as f64 / 1048576.0 / up.as_secs_f64(), obj.id());

    // ranged read straddling a slab boundary (slabs are 40 MiB of data at 10-of-30;
    // use the middle of the object and an odd length)
    let off = (n / 2 - 12345) as u64;
    let len = 1_000_003u64.min((n as u64) - off);
    let t = Instant::now();
    let mut rd = sdk.download(&obj, DownloadOptions { offset: off, length: Some(len), ..Default::default() })?;
    let mut got = Vec::new();
    rd.read_to_end(&mut got).await?;
    let dt = t.elapsed();
    if got != data[off as usize..(off + len) as usize] {
        return Err(format!("range mismatch: got {} bytes", got.len()).into());
    }
    println!("ranged download {len} bytes @ {off} OK in {dt:?} (TTFB+transfer)");

    let t = Instant::now();
    let mut rd = sdk.download(&obj, DownloadOptions::default())?;
    let mut all = Vec::new();
    rd.read_to_end(&mut all).await?;
    let dt = t.elapsed();
    if all != data {
        return Err("full download mismatch".into());
    }
    println!("full download {n} bytes OK in {dt:?} ({:.2} MiB/s)", n as f64 / 1048576.0 / dt.as_secs_f64());

    // list: the object must be visible, metadata intact, via the indexer alone
    let mut cursor = None;
    let mut seen = false;
    let mut total = 0;
    loop {
        let evs = sdk.object_events(cursor.take(), Some(500)).await?;
        if evs.is_empty() {
            break;
        }
        for e in &evs {
            total += 1;
            if e.id == obj.id() {
                let md = e.object.as_ref().map(|o| String::from_utf8_lossy(&o.metadata).into_owned());
                println!("listed via object_events: deleted={} metadata={md:?}", e.deleted);
                seen = true;
            }
        }
        let last = evs.last().unwrap();
        cursor = Some(sia_storage::ObjectsCursor { id: last.id, after: last.updated_at });
        if evs.len() < 500 {
            break;
        }
    }
    if !seen {
        return Err(format!("object not in object_events ({total} events)").into());
    }

    sdk.delete_object(&obj.id()).await?;
    println!("deleted; PASS");
    Ok(())
}

fn getrandom(buf: &mut [u8]) {
    // xorshift seeded from the clock: the bytes only need to be incompressible
    let mut x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 | 1;
    for c in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        c.copy_from_slice(&b[..c.len()]);
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> R<[u8; 32]> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return Err("bad key length".into());
    }
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?;
    }
    Ok(out)
}

/// RHP4 settings over SiaMux to real hosts: dial (through ENCLAVE_EGRESS when
/// set), authenticate the host key in the mux handshake, fetch signed prices.
async fn settings(hostsfile: &str) -> R<()> {
    use sia_core::rhp4::protocol::RPCSettings;
    let hosts: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(hostsfile)?)?;
    let (mut ok, mut bad) = (0, 0);
    for h in &hosts {
        let addr = h["addr"].as_str().unwrap_or_default();
        let key: [u8; 32] = unhex(h["key"].as_str().unwrap_or_default())?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&key)?;
        let t = Instant::now();
        let r = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let tcp = reqwest_wasi::net::connect_addr(addr).await?;
            let mux = sia_mux::dial(tcp, &vk).await.map_err(|e| format!("mux: {e}"))?;
            let mut st = mux.dial_stream().map_err(|e| format!("stream: {e}"))?;
            let resp = RPCSettings::send_request(&mut st).await?.complete(&mut st).await?;
            Ok::<_, Box<dyn std::error::Error>>(resp.settings)
        })
        .await;
        match r {
            Ok(Ok(s)) => {
                ok += 1;
                println!("OK   {:>6.0?} {:<2} {addr:<40} release={} accepting={} storage/TB/mo~{:?}",
                    t.elapsed(), h["cc"].as_str().unwrap_or("?"), s.release, s.accepting_contracts, s.prices.storage_price);
            }
            Ok(Err(e)) => { bad += 1; println!("FAIL {:>6.0?}    {addr:<40} {e}", t.elapsed()); }
            Err(_) => { bad += 1; println!("TIME          {addr}"); }
        }
    }
    println!("settings: {ok} ok, {bad} failed of {}", hosts.len());
    if ok == 0 { return Err("no host answered".into()); }
    Ok(())
}
