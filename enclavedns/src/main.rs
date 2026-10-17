//! EnclaveDNS: a validating DNSSEC resolver and signing authoritative
//! server that also resolves `.eth` (ENS) and `.onion` natively, as one
//! wasm32-wasip2 service app.
//!
//! Listeners (logical ports; the platform maps each to an actual port in
//! ENCLAVE_PORTS): udp:53 and tcp:53 (plain DNS, reached at the
//! deployment's dedicated IPv6), tcp:853 (DNS-over-TLS: the platform ends
//! TLS inside the enclave and hands us the DNS-over-TCP stream), and
//! http:8000 (DNS-over-HTTPS, the JSON API and the status page).

mod auth;
mod config;
mod dnssec;
mod ens;
mod http;
mod name;
mod net;
mod onion;
mod rdata;
mod resolver;
mod server;
mod tls;
mod wire;

use server::{Admit, Server, Via};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

fn spawn(f: impl std::future::Future<Output = ()> + 'static) {
    wstd::runtime::spawn(f).detach();
}

async fn udp_loop(server: Rc<Server>, sock: Rc<net::Udp>) {
    loop {
        let (wire, from) = match sock.recv().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[enclavedns] udp receive: {e}");
                net::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let reply = match server.admit_udp(&from.to_string()) {
            Admit::Yes => None,
            Admit::Slip => Some(Server::slip_reply(&wire)),
            Admit::Drop => continue,
        };
        let (server, sock) = (server.clone(), sock.clone());
        spawn(async move {
            let out = match reply {
                Some(r) => r,
                None => server.handle(&wire, Via::Udp).await,
            };
            if let Some(out) = out {
                match sock.send_to(&out, from).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        if !server.udp_replies_blocked.replace(true) {
                            eprintln!(
                                "[enclavedns] this host refuses UDP sends from the app (transparent egress is on), so UDP/53 cannot answer; TCP/53, DNS-over-TLS and DNS-over-HTTPS are unaffected"
                            );
                        }
                    }
                    Err(e) => eprintln!("[enclavedns] udp send to {from}: {e}"),
                }
            }
        });
    }
}

/// DNS over a stream (RFC 7766): length-prefixed messages, answered in
/// order, idle connections closed after 30 s.
async fn stream_conn(server: Rc<Server>, conn: net::Tcp, via: Via) {
    loop {
        let len = match net::timeout(Duration::from_secs(30), conn.read_exact(2)).await {
            Ok(l) => u16::from_be_bytes([l[0], l[1]]) as usize,
            Err(_) => break,
        };
        let Ok(wire) = net::timeout(Duration::from_secs(10), conn.read_exact(len)).await else { break };
        let Some(out) = server.handle(&wire, via).await else { break };
        let mut framed = (out.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&out);
        if conn.write_all(&framed).await.is_err() {
            break;
        }
    }
    conn.shutdown();
}

async fn tcp_listener(server: Rc<Server>, l: net::Listener, via: Via) {
    loop {
        match l.accept().await {
            Ok(c) => spawn(stream_conn(server.clone(), c, via)),
            Err(e) => {
                eprintln!("[enclavedns] accept: {e}");
                net::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn http_listener(app: Rc<http::App>, l: net::Listener) {
    loop {
        match l.accept().await {
            Ok(c) => spawn(http::serve_conn(app.clone(), c)),
            Err(e) => {
                eprintln!("[enclavedns] http accept: {e}");
                net::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn die(msg: String) -> ! {
    eprintln!("[enclavedns] {msg}");
    std::process::exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // A config that does not load is reported, not fatal: on an isolated host the app's stderr is withheld, so the
    // status page is the only place the reason can be read. The server then answers with nothing but its own status.
    let mut startup: Vec<(String, Result<String, String>)> = Vec::new();
    let mut cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[enclavedns] config: {e}");
            startup.push(("config".into(), Err(e)));
            let safe = serde_json::json!({"recursion": false, "ens": {"enabled": false}, "onion": {"enabled": false}});
            config::Config::from_json(&safe, std::env::var("ENCLAVE_PORTS").ok().as_deref()).unwrap_or_else(|e| die(e))
        }
    };
    // a smaller guest gets a smaller cache unless one is configured
    if let Some(mb) = std::env::var("ENCLAVE_MEM_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
        cfg.resolver.cache_entries = cfg.resolver.cache_entries.min(mb * 150).max(1000);
    }
    wstd::runtime::block_on(async move {
        let pool = Rc::new(tls::Pool::default());
        let resolver = resolver::Resolver::new(cfg.resolver.clone(), pool);
        let ens = cfg.ens.clone().map(|c| ens::Ens::new(c, resolver.clone()));
        let mut zones = Vec::new();
        for z in &cfg.zones {
            match auth::Zone::build(z, cfg.zone_seed.as_deref()) {
                Ok(zone) => {
                    if zone.ephemeral_key {
                        eprintln!(
                            "[enclavedns] {}: no zone_key_seed secret, so its signing key is new on every start and a DS at the parent will stop matching",
                            zone.apex
                        );
                    }
                    if let Some(ds) = zone.ds_text() {
                        println!("[enclavedns] zone {} signed; publish at the parent: {ds}", zone.apex);
                    } else {
                        println!("[enclavedns] zone {} (unsigned)", zone.apex);
                    }
                    startup.push((format!("zone {}", zone.apex), Ok(format!("{} records", zone.record_count()))));
                    zones.push(zone);
                }
                Err(e) => {
                    eprintln!("[enclavedns] zone {}: {e}", z.name);
                    startup.push((format!("zone {}", z.name), Err(e)));
                }
            }
        }
        let server = Server::new(resolver, zones, ens, cfg.ens_alias, cfg.onion.clone(), cfg.recursion, cfg.udp_rate, cfg.udp_burst);

        // `enclavedns lookup <name> [type] ...`: resolve and print, then exit
        if args.get(1).map(|s| s.as_str()) == Some("lookup") {
            for pair in args[2..].chunks(2) {
                let n = name::Name::parse(&pair[0]).unwrap_or_else(|e| die(e));
                let t = wire::ty::parse(pair.get(1).map(|s| s.as_str()).unwrap_or("A")).unwrap_or_else(|| die("bad type".into()));
                let t0 = net::now_ms();
                let r = server.resolve(&n, t, true, true).await;
                println!(
                    "{n} {} -> {} {} [{}] {} ms{}",
                    wire::ty::name(t),
                    wire::rcode::name(r.rcode),
                    r.sec.name(),
                    r.sources.join(","),
                    net::now_ms() - t0,
                    r.ede.map(|(c, w)| format!(" (EDE {c}: {w})")).unwrap_or_default()
                );
                for rr in r.answer.iter().filter(|x| x.rtype != wire::ty::RRSIG) {
                    println!("   {} {} {} {}", rr.name, rr.ttl, wire::ty::name(rr.rtype), rdata::to_text(rr.rtype, &rr.data));
                }
            }
            return;
        }

        let bind = cfg.bind;
        let p = cfg.ports.clone();
        let mut up = Vec::new();
        // each listener on its own: one that cannot bind is reported and the rest still serve
        let mut note = |what: String, r: Result<(), std::io::Error>| match r {
            Ok(()) => {
                up.push(what.clone());
                startup.push((what, Ok("listening".into())));
            }
            Err(e) => {
                eprintln!("[enclavedns] {what}: {e}");
                startup.push((what, Err(format!("{e} ({:?})", e.kind()))));
            }
        };
        if let Some(port) = p.dns_udp {
            let r = net::Udp::bind(SocketAddr::new(bind, port)).await.map(|s| spawn(udp_loop(server.clone(), Rc::new(s))));
            note(format!("dns udp {bind}:{port}"), r);
        }
        if let Some(port) = p.dns_tcp {
            let r = net::Listener::bind(SocketAddr::new(bind, port)).await.map(|l| spawn(tcp_listener(server.clone(), l, Via::Tcp)));
            note(format!("dns tcp {bind}:{port}"), r);
        }
        if let Some(port) = p.dot {
            let r = net::Listener::bind(SocketAddr::new(bind, port)).await.map(|l| spawn(tcp_listener(server.clone(), l, Via::Tls)));
            note(format!("dot {bind}:{port} (tls ended by the platform)"), r);
        }
        let app = Rc::new(http::App { server: server.clone(), admin_token: cfg.admin_token.clone() });
        if let Some(port) = p.http {
            let r = net::Listener::bind(SocketAddr::new(bind, port)).await.map(|l| spawn(http_listener(app, l)));
            note(format!("http {bind}:{port}"), r);
        }
        *server.startup.borrow_mut() = startup;
        if up.is_empty() {
            die("no listener could bind (see above); declare udp:53, tcp:53, tcp:853 and/or http:8000".into());
        }
        println!("[enclavedns] {} up: {}; {}", env!("CARGO_PKG_VERSION"), up.join(", "), server.resolver.transport_text());
        // prime: the root keys (and with them the transport), then ENS's hosts
        let s2 = server.clone();
        spawn(async move {
            let ctx = resolver::Ctx::new(Duration::from_secs(20));
            let root = s2.resolver.lookup(&name::Name::root(), wire::ty::DNSKEY, &ctx, 0).await;
            println!("[enclavedns] root keys {}; {}", root.sec.name(), s2.resolver.transport_text());
            if let Some(e) = &s2.ens {
                e.warm().await;
            }
        });
        // the listeners run as tasks; this one only keeps the reactor alive
        loop {
            net::sleep(Duration::from_secs(3600)).await;
        }
    })
}
