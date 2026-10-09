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

use std::io::{Read, Write};
use std::time::Duration;

/// One attempt to open the bucket: the registry and the token book, which
/// also proves the master key. A key that cannot open what is stored, or a
/// format from a newer server, ends the process (it would never get better);
/// anything else (storage or its route not there yet) is retried with backoff.
fn open_storage(app: &mut app::App, attempt: &mut u32, next_try: &mut std::time::Instant) {
    let quick = |app: &mut app::App, q: bool| {
        app.store.s3.set_quick(q);
        if let Some(w) = &mut app.store.witness {
            w.s3.set_quick(q);
        }
    };
    quick(app, true);
    let r = app
        .refresh_registry(true)
        .and_then(|_| app.tokens.refresh(&mut app.store, true));
    quick(app, false);
    match r {
        Ok(()) => {
            app.ready = true;
            println!(
                "[depot] storage open: {} repositories",
                app.reg.repos.len()
            );
        }
        // storage is older than the witness: serve, refusing what is
        // affected, so an admin can look and decide (POST /api/witness)
        Err(e) if app.store.witness.as_ref().is_some_and(|w| !w.rolled.is_empty()) => {
            eprintln!("[depot] ROLLBACK DETECTED: {e}");
            app.ready = true;
        }
        Err(e)
            if e.contains("cannot open a stored object")
                || e.contains("does not parse")
                || e.contains("newer than this server") =>
        {
            eprintln!("[depot] cannot open the registry: {e}");
            std::process::exit(1);
        }
        Err(e) => {
            *attempt += 1;
            let wait = Duration::from_secs((1u64 << (*attempt).min(4)).min(15));
            eprintln!(
                "[depot] storage not ready ({e}); retrying in {}s",
                wait.as_secs()
            );
            *next_try = std::time::Instant::now() + wait;
        }
    }
}

/// A platform host may launch a deployment once without its secrets: the
/// relay releases them only to the host holding the lease, and the host takes
/// the lease once the app serves, then relaunches it with them. Serve that
/// wait and nothing else (storage is not touched): /ping answers, every other
/// request is told to come back.
fn wait_for_secrets(why: &str) -> ! {
    let port = serve::resolve_port(8080);
    let host = std::env::var("DEPOT_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let listener = match std::net::TcpListener::bind((host.as_str(), port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[depot] bind {host}:{port}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[depot] {why}: waiting for this deployment's secrets (storage untouched)");
    println!(
        "[depot] {} waiting for its secrets on port {port}",
        env!("CARGO_PKG_VERSION")
    );
    for conn in listener.incoming() {
        let Ok(mut s) = conn else { continue };
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 1024];
        let n = s.read(&mut buf).unwrap_or(0);
        let head = &buf[..n];
        let (status, ctype, body) = if head.starts_with(b"GET /ping ") || head.starts_with(b"HEAD /ping ") {
            ("200 OK", "text/plain", "ok\n")
        } else {
            (
                "503 Service Unavailable",
                "application/json",
                "{\"error\":\"this deployment is waiting for its secrets; try again shortly\"}",
            )
        };
        let resp = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nretry-after: 30\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{}",
            body.len(),
            if head.starts_with(b"HEAD ") { "" } else { body }
        );
        let _ = s.write_all(resp.as_bytes());
    }
    std::process::exit(1)
}

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
        Err(e) if config::missing_secret(&e) => wait_for_secrets(&e),
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
    // The port opens first and storage is reached from the loop: a platform
    // host waits only so long for the port, and a host may only route this
    // deployment's egress (its way to storage) once it serves. Until storage
    // answers, every request is told to come back (see routes.rs).
    let mut server: serve::Server<app::App> =
        match serve::Server::bind(concat!("depot/", env!("CARGO_PKG_VERSION")), 8080) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[depot] {e}");
                std::process::exit(1);
            }
        };
    println!(
        "[depot] {} listening on port {}, opening bucket {}",
        env!("CARGO_PKG_VERSION"),
        server.port,
        app.cfg.storage.bucket
    );
    let mut attempt = 0u32;
    let mut next_try = std::time::Instant::now();
    loop {
        if !app.ready && std::time::Instant::now() >= next_try {
            open_storage(&mut app, &mut attempt, &mut next_try);
        }
        let busy = server.step(&mut app);
        // maintenance only in the gaps between requests
        let worked = !busy && app.ready && (hooks::tick(&mut app) || maint::tick(&mut app));
        if !busy && !worked {
            std::thread::sleep(Duration::from_millis(if server.connections() > 0 {
                2
            } else {
                10
            }));
        }
    }
}
