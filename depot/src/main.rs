//! depot: a git server whose storage never sees plaintext.

mod api;
mod app;
mod client;
mod config;
mod egress;
mod fetch;
mod gc;
mod git;
mod hooks;
mod maint;
mod push;
mod routes;
mod s3;
mod seal;
mod serve;
mod sso;
mod store;
mod tokens;
mod witness;

use std::time::Duration;

fn main() {
    // DEPOT_PROBE=https://host: one GET over this build's TLS and egress path, for diagnosing storage reachability
    if let Ok(url) = std::env::var("DEPOT_PROBE") {
        let r = client::Client::new(&url).and_then(|mut c| c.send("GET", "/", &[], &[]));
        match r {
            Ok(r) => {
                println!(
                    "[depot] probe {url}: HTTP {} ({} bytes)",
                    r.status,
                    r.body.len()
                );
                std::process::exit(0);
            }
            Err(e) => {
                println!("[depot] probe {url}: {e}");
                std::process::exit(1);
            }
        }
    }
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[depot] configuration: {e}");
            std::process::exit(1);
        }
    };
    let st = &cfg.storage;
    let s3 = match s3::S3::new(
        &st.endpoint,
        &st.region,
        &st.bucket,
        &st.access_key,
        &st.secret_key,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[depot] storage: {e}");
            std::process::exit(1);
        }
    };
    let mut store = store::Store::new(s3, &cfg.master_key, &st.prefix, cfg.cache_mb);
    if let Some(w) = &cfg.witness {
        match s3::S3::new(&w.endpoint, &w.region, &w.bucket, &w.access_key, &w.secret_key) {
            Ok(s) => store.set_witness(s, &w.prefix),
            Err(e) => {
                eprintln!("[depot] witness: {e}");
                std::process::exit(1);
            }
        }
    }
    let mut app = app::App::new(cfg, store);
    // storage must answer (and the master key must open the registry) before we serve
    for attempt in 0.. {
        match app
            .refresh_registry(true)
            .and_then(|_| app.tokens.refresh(&mut app.store, true))
        {
            Ok(()) => break,
            // storage is older than the witness: serve, refusing what is
            // affected, so an admin can look and decide (POST /api/witness)
            Err(e) if app.store.witness.as_ref().is_some_and(|w| !w.rolled.is_empty()) => {
                eprintln!("[depot] ROLLBACK DETECTED: {e}");
                break;
            }
            Err(e) if attempt < 5 => {
                eprintln!("[depot] storage not ready ({e}); retrying");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => {
                eprintln!("[depot] cannot open the registry: {e}");
                std::process::exit(1);
            }
        }
    }
    let mut server: serve::Server<app::App> =
        match serve::Server::bind(concat!("depot/", env!("CARGO_PKG_VERSION")), 8080) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[depot] {e}");
                std::process::exit(1);
            }
        };
    println!(
        "[depot] {} listening on port {} ({} repositories, bucket {})",
        env!("CARGO_PKG_VERSION"),
        server.port,
        app.reg.repos.len(),
        app.cfg.storage.bucket
    );
    loop {
        let busy = server.step(&mut app);
        // maintenance only in the gaps between requests
        let worked = !busy && (hooks::tick(&mut app) || maint::tick(&mut app));
        if !busy && !worked {
            std::thread::sleep(Duration::from_millis(if server.connections() > 0 {
                2
            } else {
                10
            }));
        }
    }
}
