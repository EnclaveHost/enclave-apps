mod api;
mod config;
mod crypt;
mod egress;
mod engine;
mod execute;
mod httpd;
mod net;
mod schedule;
mod sigv4;
mod sso;
mod store;
use engine::{Run, State};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Clock before epoch")
        .as_secs()
}
fn commit(store: &mut store::Store, state: &mut State) {
    if let Err(e) = store.save(state, now()) {
        eprintln!("[cron] FENCED: {e}. No further work will be dispatched.");
        std::process::exit(1);
    }
    if now() >= state.lease_until {
        eprintln!("[cron] Lease expired during commit; stopping");
        std::process::exit(1);
    }
}
fn dispatch(
    cfg: &config::Config,
    store: &mut store::Store,
    state: &mut State,
    active: &mut BTreeMap<String, (Run, net::Pending)>,
    r: Run,
) {
    if now() >= state.lease_until {
        std::process::exit(1);
    }
    match execute::start(cfg, &r) {
        Ok(p) => {
            active.insert(r.id.clone(), (r, p));
        }
        Err(e) => {
            state.finish(&r.id, now(), "failed", e, None);
            commit(store, state);
        }
    }
}
fn main() {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[cron] Configuration: {e}");
            std::process::exit(1)
        }
    };
    let mut store = store::Store::new(cfg.storage.clone()).expect("Randomness required");
    let mut state = match store.load(now()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[cron] Cannot acquire durable state: {e}");
            std::process::exit(1)
        }
    };
    let mut server = httpd::Server::bind("enclave-cron/0.1.0", 8080);
    let mut active: BTreeMap<String, (Run, net::Pending)> = BTreeMap::new();
    let mut last_tick = 0;
    let mut last_renew = now();
    loop {
        if now() >= state.lease_until {
            eprintln!("[cron] Lease expired; stopping without replay");
            std::process::exit(1);
        }
        let mut finished = Vec::new();
        for (id, (run, p)) in &mut active {
            if let Some(r) = p.poll() {
                let (status, text, code) = match r {
                    Ok(reply) => execute::result(&run.action, reply),
                    Err(e) => ("failed".into(), e, None),
                };
                finished.push((id.clone(), status, text, code));
            }
        }
        if !finished.is_empty() {
            for (id, status, mut text, code) in finished {
                if let Some((run, _)) = active.remove(&id) {
                    if let Ok(t) = cfg.target(run.action.target(), &run.owner) {
                        for secret in t.headers.values().chain(t.api_keys.values()) {
                            if secret.len() >= 8 {
                                text = text.replace(secret, "[redacted]");
                            }
                        }
                    }
                }
                state.finish(&id, now(), &status, text, code);
            }
            commit(&mut store, &mut state);
            last_renew = now();
        }
        for (conn, req) in server.poll(32768) {
            let mut candidate = state.clone();
            let out = api::route(
                &cfg,
                &mut candidate,
                &req,
                now(),
                cfg.concurrency.saturating_sub(active.len()),
            );
            if out.changed {
                commit(&mut store, &mut candidate);
                state = candidate;
                last_renew = now();
            }
            server.respond(conn, out.response);
            if let Some(run) = out.launch {
                dispatch(&cfg, &mut store, &mut state, &mut active, run);
            }
        }
        let t = now();
        if t != last_tick {
            last_tick = t;
            let mut candidate = state.clone();
            let before = serde_json::to_vec(&candidate).unwrap();
            let launches = candidate.claim_due(t, cfg.concurrency.saturating_sub(active.len()));
            if !launches.is_empty() || before != serde_json::to_vec(&candidate).unwrap() {
                commit(&mut store, &mut candidate);
                state = candidate;
                last_renew = now();
                for r in launches {
                    dispatch(&cfg, &mut store, &mut state, &mut active, r);
                }
            }
        }
        if now().saturating_sub(last_renew) >= 30 {
            commit(&mut store, &mut state);
            last_renew = now();
        }
        server.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
}
