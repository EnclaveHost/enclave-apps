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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Clock before epoch")
        .as_secs()
}
fn commit(store: &mut store::Store, state: &mut State) -> Result<(), String> {
    store.save(state, now())?;
    if now() >= state.lease_until {
        return Err("Lease expired during commit".into());
    }
    Ok(())
}
fn dispatch(
    cfg: &config::Config,
    store: &mut store::Store,
    state: &mut State,
    active: &mut BTreeMap<String, (Run, net::Pending)>,
    r: Run,
) -> Result<(), String> {
    if now() >= state.lease_until {
        return Err("Lease expired before dispatch".into());
    }
    match execute::start(cfg, &r) {
        Ok(p) => {
            active.insert(r.id.clone(), (r, p));
        }
        Err(e) => {
            state.finish(&r.id, now(), "failed", e, None);
            commit(store, state)?;
        }
    }
    Ok(())
}
fn main() {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[cron] Configuration: {e}");
            std::process::exit(1)
        }
    };
    // Keep the listener (and therefore the partition's TLS key) alive while storage
    // is unavailable. Fencing ends an ownership session, not the process. Never
    // reuse its dirty state or ETag: recovery reloads and acquires via CAS only
    // after the durable lease expires, marking unfinished work interrupted.
    let mut server = httpd::Server::bind("enclave-cron", 8080);
    let mut retry = 1;
    loop {
        let mut store = store::Store::new(cfg.storage.clone()).expect("Randomness required");
        let result = match store.load(now()) {
            Ok(state) => {
                retry = 1;
                run_owned(&cfg, &mut store, state, &mut server)
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            eprintln!(
                "[cron] FENCED: {e}. No work dispatched until durable ownership is reacquired."
            );
        }
        let until = Instant::now() + Duration::from_secs(retry);
        while Instant::now() < until {
            for (conn, req) in server.poll(32768) {
                server.respond(conn, api::unavailable(&req));
            }
            server.flush();
            std::thread::sleep(Duration::from_millis(20));
        }
        retry = (retry * 2).min(30);
    }
}

fn run_owned(
    cfg: &config::Config,
    store: &mut store::Store,
    mut state: State,
    server: &mut httpd::Server,
) -> Result<(), String> {
    let mut active: BTreeMap<String, (Run, net::Pending)> = BTreeMap::new();
    let mut last_tick = 0;
    let mut last_renew = now();
    loop {
        if now() >= state.lease_until {
            return Err("Lease expired; fencing without replay".into());
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
            commit(store, &mut state)?;
            last_renew = now();
        }
        let mut requests = server.poll(32768).into_iter();
        while let Some((conn, req)) = requests.next() {
            let mut candidate = state.clone();
            let out = api::route(
                cfg,
                &mut candidate,
                &req,
                now(),
                cfg.concurrency.saturating_sub(active.len()),
            );
            if out.changed {
                if let Err(e) = commit(store, &mut candidate) {
                    server.respond(conn, api::unavailable(&req));
                    for (conn, req) in requests {
                        server.respond(conn, api::unavailable(&req));
                    }
                    server.flush();
                    return Err(e);
                }
                state = candidate;
                last_renew = now();
            }
            server.respond(conn, out.response);
            if let Some(run) = out.launch {
                if let Err(e) = dispatch(cfg, store, &mut state, &mut active, run) {
                    for (conn, req) in requests {
                        server.respond(conn, api::unavailable(&req));
                    }
                    server.flush();
                    return Err(e);
                }
            }
        }
        let t = now();
        if t != last_tick {
            last_tick = t;
            let mut candidate = state.clone();
            let before = serde_json::to_vec(&candidate).unwrap();
            let launches = candidate.claim_due(t, cfg.concurrency.saturating_sub(active.len()));
            if !launches.is_empty() || before != serde_json::to_vec(&candidate).unwrap() {
                commit(store, &mut candidate)?;
                state = candidate;
                last_renew = now();
                for r in launches {
                    dispatch(cfg, store, &mut state, &mut active, r)?;
                }
            }
        }
        if now().saturating_sub(last_renew) >= 30 {
            commit(store, &mut state)?;
            last_renew = now();
        }
        server.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
}
