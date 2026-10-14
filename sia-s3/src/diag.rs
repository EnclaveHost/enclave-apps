//! `GET /?sia-diag` (SigV4-signed like every request): dial the Sia hosts the
//! indexer offers through the SAME path uploads use (reqwest_wasi::net, so the
//! deployment's SOCKS egress front when ENCLAVE_EGRESS is set) and report, per
//! host, which stage failed and how long it took:
//!   tcp      - TCP (via the SOCKS front) to the announced siamux address
//!   mux      - SiaMux handshake, which authenticates the host's key
//!   settings - an RHP4 settings RPC over the mux
//! A deployment that can read the indexer but cannot upload ("no more hosts
//! available") shows here exactly where the dials die.

use std::time::{Duration, Instant};

use serde::Serialize;
use sia_core::rhp4::protocol::RPCSettings;
use sia_core::types::v2::Protocol;
use sia_storage::HostQuery;

use crate::store::Store;

#[derive(Serialize)]
struct Probe {
    host: String,
    addr: String,
    stage: &'static str,
    ok: bool,
    ms: u128,
    err: Option<String>,
}

fn egress_summary() -> serde_json::Value {
    match std::env::var("ENCLAVE_EGRESS") {
        Ok(u) if !u.is_empty() => {
            // never echo the credential: scheme + front address only
            let scheme = u.split("://").next().unwrap_or("");
            let front = u.rsplit('@').next().unwrap_or("");
            serde_json::json!({ "set": true, "scheme": scheme, "front": front })
        }
        _ => serde_json::json!({ "set": false }),
    }
}

async fn probe(pk: [u8; 32], host: String, addr: String, per_dial: Duration) -> Probe {
    let t = Instant::now();
    let fail = |stage, e: String| Probe { host: host.clone(), addr: addr.clone(), stage, ok: false, ms: t.elapsed().as_millis(), err: Some(e) };
    let vk = match ed25519_dalek::VerifyingKey::from_bytes(&pk) {
        Ok(k) => k,
        Err(e) => return fail("key", e.to_string()),
    };
    let tcp = match tokio::time::timeout(per_dial, reqwest_wasi::net::connect_addr(&addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return fail("tcp", e.to_string()),
        Err(_) => return fail("tcp", format!("timeout after {per_dial:?}")),
    };
    let mux = match tokio::time::timeout(per_dial, sia_mux::dial(tcp, &vk)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return fail("mux", e.to_string()),
        Err(_) => return fail("mux", format!("timeout after {per_dial:?}")),
    };
    let res = tokio::time::timeout(per_dial, async {
        let mut st = mux.dial_stream().map_err(|e| e.to_string())?;
        let req = RPCSettings::send_request(&mut st).await.map_err(|e| e.to_string())?;
        req.complete(&mut st).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })
    .await;
    match res {
        Ok(Ok(())) => Probe { host, addr, stage: "settings", ok: true, ms: t.elapsed().as_millis(), err: None },
        Ok(Err(e)) => fail("settings", e),
        Err(_) => fail("settings", format!("timeout after {per_dial:?}")),
    }
}

pub async fn run(store: &Store, limit: u64, per_dial_secs: u64) -> String {
    let started = Instant::now();
    let hosts = match store.sdk.hosts(HostQuery { limit: Some(limit), ..Default::default() }).await {
        Ok(h) => h,
        Err(e) => return serde_json::json!({ "egress": egress_summary(), "error": format!("indexer hosts: {e}") }).to_string(),
    };
    let per_dial = Duration::from_secs(per_dial_secs);
    let mut set = tokio::task::JoinSet::new();
    let mut no_siamux = 0;
    for h in hosts.iter() {
        let pk: [u8; 32] = h.public_key.into();
        let Some(a) = h.addresses.iter().find(|a| a.protocol == Protocol::SiaMux) else {
            no_siamux += 1;
            continue;
        };
        set.spawn_local(probe(pk, h.public_key.to_string(), a.address.clone(), per_dial));
    }
    let mut probes = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(p) = r {
            probes.push(p);
        }
    }
    probes.sort_by(|a, b| b.ok.cmp(&a.ok).then(a.stage.cmp(b.stage)));
    let ok = probes.iter().filter(|p| p.ok).count();
    let mut by_stage = std::collections::BTreeMap::new();
    for p in probes.iter().filter(|p| !p.ok) {
        *by_stage.entry(p.stage).or_insert(0) += 1;
    }
    serde_json::json!({
        "egress": egress_summary(),
        "hostsOffered": hosts.len(),
        "withoutSiamux": no_siamux,
        "ok": ok,
        "failedByStage": by_stage,
        "elapsedMs": started.elapsed().as_millis(),
        "probes": probes,
    })
    .to_string()
}
