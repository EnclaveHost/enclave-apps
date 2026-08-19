//! ipns-publisher — sign IPNS records inside the enclave, publish them out.
//!
//! The ed25519 name key arrives as a deployment secret and never leaves the
//! TEE; every publish path is outbound-only. Three publish surfaces, in
//! order of increasing self-sufficiency:
//!   1. this app serves the signed record over HTTP (delegated-routing GET
//!      shape), so anything that can reach the deployment can resolve;
//!   2. the record is PUT to delegated-routing endpoints (IPIP-379), so
//!      public resolvers pick it up without this app being a peer;
//!   3. (milestones 4-6) the record is PUT straight onto the public IPFS
//!      DHT over a hand-rolled outbound-only libp2p stack.
//!
//! One single-threaded event loop (wasm32-wasip2 has no threads): the HTTP
//! engine polls, and the publisher advances at most one bounded network
//! task per tick, the s3-ipfs-adapter discipline.

mod egress;
mod httpd;
mod ipni;
mod ipns;
mod kad;
mod multiformats;
mod noise;
mod p2p;
mod webreq;
mod yamux;

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use httpd::{json, json_escape, Request, Response, Server};
use ipns::Identity;

const APP: &str = "ipns-publisher/0.1.0";
const MAX_BODY: usize = 64 * 1024;
const STATE_FILE: &str = "/data/ipns-publisher-state.json";

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64
}

// ---- config ----------------------------------------------------------------

struct Config {
    identity: Option<Identity>,
    key_error: Option<String>,
    value_src: String,
    lifetime_secs: u64,
    republish_secs: u64,
    ttl_secs: u64,
    delegates: Vec<String>,
    bootstrap: Vec<String>,
    dht: bool,
    api_key: String,
}

/// The public bootstrap set, filtered to what Step 0 allows: TCP only.
/// (The fleet egress front is SOCKS5 CONNECT — no UDP, so no QUIC.)
/// These are the long-lived Protocol Labs bootstrappers; config `bootstrap`
/// replaces the list wholesale.
const DEFAULT_BOOTSTRAP: &[&str] = &[
    // the canonical bootstrap.libp2p.io set, in its stable /dns/…/tcp/4001
    // form (the /dnsaddr umbrella needs TXT lookups the fleet cannot do)
    "/dns/am6.bootstrap.libp2p.io/tcp/4001/p2p/QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb",
    "/dns/ny5.bootstrap.libp2p.io/tcp/4001/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "/dns/sg1.bootstrap.libp2p.io/tcp/4001/p2p/QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt",
    "/dns/sv15.bootstrap.libp2p.io/tcp/4001/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
];

fn expand_env_refs(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            let Some(reference) = s.strip_prefix('$') else { return };
            let name = reference.strip_prefix('{').and_then(|r| r.strip_suffix('}')).unwrap_or(reference);
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || name.starts_with(|c: char| c.is_ascii_digit())
            {
                return;
            }
            match std::env::var(name) {
                Ok(val) => {
                    eprintln!("[ipns-publisher] config: resolved ${name} from the environment");
                    *s = val;
                }
                Err(_) => {
                    eprintln!("[ipns-publisher] config: ${name} is not set; treating the value as absent");
                    s.clear();
                }
            }
        }
        serde_json::Value::Object(map) => map.values_mut().for_each(expand_env_refs),
        serde_json::Value::Array(items) => items.iter_mut().for_each(expand_env_refs),
        _ => {}
    }
}

fn load_config() -> Config {
    let raw = std::env::var("ENCLAVE_CONFIG")
        .or_else(|_| std::env::var("IPNSPUB_CONFIG"))
        .unwrap_or_default();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        if !raw.is_empty() {
            eprintln!("[ipns-publisher] config is not JSON ({e}); starting unconfigured");
        }
        serde_json::Value::Null
    });
    expand_env_refs(&mut v);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
    let n = |k: &str, d: u64| v.get(k).and_then(|x| x.as_u64()).unwrap_or(d);
    let list = |k: &str| -> Option<Vec<String>> {
        v.get(k)?.as_array().map(|a| {
            a.iter().filter_map(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string).collect()
        })
    };
    let (identity, key_error) = match s("ipnsKey") {
        Some(k) => match Identity::parse(&k) {
            Ok(id) => (Some(id), None),
            Err(e) => (None, Some(e)),
        },
        None => (None, Some("ipnsKey is not configured".to_string())),
    };
    Config {
        identity,
        key_error,
        value_src: s("value").unwrap_or_default(),
        lifetime_secs: n("lifetimeSecs", 48 * 3600),
        republish_secs: n("republishSecs", 4 * 3600).max(60),
        ttl_secs: n("ttlSecs", 3600).max(1),
        delegates: list("delegates").unwrap_or_else(|| vec!["https://delegated-ipfs.dev".into()]),
        bootstrap: list("bootstrap")
            .unwrap_or_else(|| DEFAULT_BOOTSTRAP.iter().map(|s| s.to_string()).collect()),
        dht: v.get("dht").and_then(|x| x.as_bool()).unwrap_or(true),
        api_key: s("api_key").unwrap_or_default(),
    }
}

// ---- publisher state machine ------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Task {
    /// Ask one delegate for the current record (sequence recovery).
    Recover(String),
    /// Recover the sequence from the DHT (a GET_VALUE walk). Non-blocking:
    /// re-enqueues itself each tick until the walk converges. `started` is
    /// false until the walk is kicked off.
    RecoverDht { started: bool },
    /// Resolve the value, choose the sequence, build + sign the record.
    Sign,
    /// PUT the current record to one delegate.
    Put(String),
    /// Kick the DHT publish machinery: non-blocking, the p2p engine advances
    /// in the event loop.
    DhtPublish,
}

struct DelegateResult {
    url: String,
    outcome: String, // "ok" or an error
    at: i64,
}

struct App {
    cfg: Config,
    tasks: VecDeque<Task>,
    record: Option<Vec<u8>>,
    sequence: u64,
    value: Vec<u8>,
    validity_unix: i64,
    published_at: Option<i64>,
    /// highest sequence seen anywhere (network recovery, state file)
    recovered_seq: Option<(u64, Vec<u8>, String)>, // (seq, value, source)
    delegate_results: Vec<DelegateResult>,
    last_error: Option<String>,
    next_publish: Instant,
    durable_state: bool,
    dht: Option<p2p::Dht>,
}

impl App {
    fn new(cfg: Config) -> App {
        let mut app = App {
            cfg,
            tasks: VecDeque::new(),
            record: None,
            sequence: 0,
            value: Vec::new(),
            validity_unix: 0,
            published_at: None,
            recovered_seq: None,
            delegate_results: Vec::new(),
            last_error: None,
            next_publish: Instant::now(),
            durable_state: false,
            dht: None,
        };
        app.load_state();
        if app.cfg.identity.is_some() {
            app.enqueue_publish();
        }
        app
    }

    fn load_state(&mut self) {
        let Ok(raw) = std::fs::read_to_string(STATE_FILE) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return };
        let seq = v.get("sequence").and_then(|x| x.as_u64());
        let value = v.get("value").and_then(|x| x.as_str());
        if let (Some(seq), Some(value)) = (seq, value) {
            eprintln!("[ipns-publisher] state file: sequence {seq}, value {value}");
            self.note_recovered(seq, value.as_bytes().to_vec(), "state-file");
            self.durable_state = true;
        }
    }

    fn save_state(&mut self) {
        let json = format!(
            "{{\"sequence\":{},\"value\":\"{}\",\"published_at\":{}}}\n",
            self.sequence,
            json_escape(&String::from_utf8_lossy(&self.value)),
            self.published_at.unwrap_or(0),
        );
        match std::fs::write(STATE_FILE, json) {
            Ok(()) => self.durable_state = true,
            Err(e) => {
                if self.durable_state {
                    eprintln!("[ipns-publisher] state file write failed: {e}");
                }
                self.durable_state = false;
            }
        }
    }

    fn note_recovered(&mut self, seq: u64, value: Vec<u8>, source: &str) {
        let better = match &self.recovered_seq {
            Some((cur, _, _)) => seq > *cur,
            None => true,
        };
        if better {
            self.recovered_seq = Some((seq, value, source.to_string()));
        }
    }

    /// Queue a full publish round: recover the sequence (from every delegate,
    /// and from the DHT when enabled) before signing, then push everywhere.
    fn enqueue_publish(&mut self) {
        if self.tasks.iter().any(|t| matches!(t, Task::Sign | Task::RecoverDht { .. })) {
            return; // a round is already queued; don't stack another
        }
        for d in &self.cfg.delegates {
            self.tasks.push_back(Task::Recover(d.clone()));
        }
        // A DHT-only deployment with no durable disk relies on this to learn
        // its last sequence; skip it when a delegate or the state file will
        // already carry the sequence (recovery is best-effort belt-and-braces
        // otherwise, and the walk costs ~20s).
        if self.cfg.dht && self.cfg.delegates.is_empty() && !self.durable_state {
            self.tasks.push_back(Task::RecoverDht { started: false });
        }
        self.tasks.push_back(Task::Sign);
    }

    /// Advance one bounded task. Returns whether work happened.
    fn tick(&mut self) -> bool {
        if self.cfg.identity.is_some() && Instant::now() >= self.next_publish {
            self.next_publish = Instant::now() + Duration::from_secs(self.cfg.republish_secs);
            self.enqueue_publish();
        }
        let Some(task) = self.tasks.pop_front() else { return false };
        match task {
            Task::Recover(delegate) => self.do_recover(&delegate),
            Task::RecoverDht { started } => self.do_recover_dht(started),
            Task::Sign => self.do_sign(),
            Task::Put(delegate) => self.do_put(&delegate),
            Task::DhtPublish => self.do_dht_publish(),
        }
        true
    }

    /// Non-blocking DHT sequence recovery: kick the GET walk on the first
    /// visit, then re-enqueue this task (ahead of Sign) until it converges.
    fn do_recover_dht(&mut self, started: bool) {
        let Some(id) = &self.cfg.identity else { return };
        let key = id.routing_key();
        let signing = id.signing.clone();
        let bootstrap = self.cfg.bootstrap.clone();
        let dht = self.dht.get_or_insert_with(|| p2p::Dht::new(bootstrap, signing));
        if !started {
            dht.start_recover(key);
            self.tasks.push_front(Task::RecoverDht { started: true });
            return;
        }
        match dht.recover_result() {
            Some(Some((seq, value))) => {
                eprintln!("[ipns-publisher] DHT recovery: sequence {seq}");
                self.note_recovered(seq, value, "dht");
            }
            Some(None) => eprintln!("[ipns-publisher] DHT recovery: no existing record (fresh name)"),
            None => {
                // still walking: yield, retry next tick, keep it before Sign
                self.tasks.push_front(Task::RecoverDht { started: true });
            }
        }
    }

    fn do_recover(&mut self, delegate: &str) {
        let Some(id) = &self.cfg.identity else { return };
        let name = id.ipns_name();
        let Some(pubkey) = ipns::peer_mh_pubkey(&id.peer_mh) else { return };
        let url = match webreq::Url::parse(delegate) {
            Ok(u) => {
                let base = u.path.trim_end_matches('/').to_string();
                u.with_path(format!("{base}/routing/v1/ipns/{name}"))
            }
            Err(e) => {
                eprintln!("[ipns-publisher] delegate {delegate}: {e}");
                return;
            }
        };
        match webreq::request("GET", &url, &[("accept", "application/vnd.ipfs.ipns-record")], &[]) {
            Ok((200, body, _)) => match ipns::verify_record(&body, &pubkey) {
                Ok(rec) => {
                    eprintln!(
                        "[ipns-publisher] {delegate}: knows sequence {} ({} bytes)",
                        rec.sequence,
                        body.len()
                    );
                    self.note_recovered(rec.sequence, rec.value, delegate);
                }
                Err(e) => eprintln!("[ipns-publisher] {delegate}: record failed verification: {e}"),
            },
            Ok((404, _, _)) => eprintln!("[ipns-publisher] {delegate}: no record yet"),
            Ok((status, _, _)) => eprintln!("[ipns-publisher] {delegate}: GET answered {status}"),
            Err(e) => eprintln!("[ipns-publisher] {delegate}: GET failed: {e}"),
        }
    }

    fn resolve_value(&self) -> Result<Vec<u8>, String> {
        let v = self.cfg.value_src.trim();
        if v.is_empty() {
            return Err("config `value` is not set".into());
        }
        if v.starts_with('/') {
            return Ok(v.as_bytes().to_vec());
        }
        if v.starts_with("http://") || v.starts_with("https://") {
            let url = webreq::Url::parse(v)?;
            let (status, body, _) = webreq::request("GET", &url, &[], &[])?;
            if status != 200 {
                return Err(format!("value source answered {status}"));
            }
            let text = String::from_utf8_lossy(&body);
            let line = text.lines().next().unwrap_or("").trim().to_string();
            if line.starts_with('/') {
                return Ok(line.into_bytes());
            }
            if !line.is_empty() && line.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Ok(format!("/ipfs/{line}").into_bytes());
            }
            return Err(format!("value source returned unusable content: {:.60}", line));
        }
        if v.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Ok(format!("/ipfs/{v}").into_bytes());
        }
        Err(format!("config `value` is neither a path, a CID, nor a URL: {v:.60}"))
    }

    fn do_sign(&mut self) {
        let value = match self.resolve_value() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ipns-publisher] publish aborted: {e}");
                self.last_error = Some(e);
                return;
            }
        };
        // Sequence: strictly increase on value change; an unchanged value
        // keeps its sequence and extends the EOL (the resolver tie-break
        // prefers the longer validity), which is kubo's behavior too.
        let (base_seq, base_value) = match &self.recovered_seq {
            Some((seq, val, _)) => (*seq, val.clone()),
            None => (0, Vec::new()),
        };
        let seq = if self.record.is_some() && self.value == value && self.sequence >= base_seq {
            self.sequence
        } else if base_value == value && (self.record.is_some() || self.recovered_seq.is_some()) {
            base_seq
        } else if self.recovered_seq.is_some() || self.record.is_some() {
            base_seq.max(self.sequence) + 1
        } else {
            0
        };
        let id = self.cfg.identity.as_ref().expect("sign task without identity");
        let validity_unix = now_unix() + self.cfg.lifetime_secs as i64;
        let validity = ipns::rfc3339(validity_unix);
        match ipns::build_record(id, &value, &validity, seq, self.cfg.ttl_secs * 1_000_000_000) {
            Ok(rec) => {
                eprintln!(
                    "[ipns-publisher] signed sequence {seq} for {} (EOL {validity})",
                    String::from_utf8_lossy(&value)
                );
                self.record = Some(rec);
                self.sequence = seq;
                self.value = value;
                self.validity_unix = validity_unix;
                self.published_at = Some(now_unix());
                self.last_error = None;
                self.note_recovered(seq, self.value.clone(), "self");
                self.save_state();
                for d in self.cfg.delegates.clone() {
                    self.tasks.push_back(Task::Put(d));
                }
                if self.cfg.dht {
                    self.tasks.push_back(Task::DhtPublish);
                }
            }
            Err(e) => {
                eprintln!("[ipns-publisher] record build failed: {e}");
                self.last_error = Some(e);
            }
        }
    }

    fn do_put(&mut self, delegate: &str) {
        let Some(record) = self.record.clone() else { return };
        let Some(id) = &self.cfg.identity else { return };
        let name = id.ipns_name();
        let outcome = (|| -> Result<String, String> {
            let u = webreq::Url::parse(delegate)?;
            let base = u.path.trim_end_matches('/').to_string();
            let url = u.with_path(format!("{base}/routing/v1/ipns/{name}"));
            let (status, body, _) = webreq::request(
                "PUT",
                &url,
                &[("content-type", "application/vnd.ipfs.ipns-record")],
                &record,
            )?;
            if status == 200 {
                Ok("ok".to_string())
            } else {
                Err(format!("PUT answered {status}: {:.120}", String::from_utf8_lossy(&body)))
            }
        })();
        let outcome = match outcome {
            Ok(s) => {
                eprintln!("[ipns-publisher] {delegate}: record accepted");
                s
            }
            Err(e) => {
                eprintln!("[ipns-publisher] {delegate}: {e}");
                e
            }
        };
        self.delegate_results.retain(|r| r.url != delegate);
        self.delegate_results.push(DelegateResult { url: delegate.into(), outcome, at: now_unix() });
    }

    fn do_dht_publish(&mut self) {
        let Some(record) = self.record.clone() else { return };
        let Some(id) = &self.cfg.identity else { return };
        let key = id.routing_key();
        let signing = id.signing.clone();
        match &mut self.dht {
            Some(dht) => dht.publish(key, record),
            None => {
                let mut dht = p2p::Dht::new(self.cfg.bootstrap.clone(), signing);
                dht.publish(key, record);
                self.dht = Some(dht);
            }
        }
    }
}

// ---- HTTP routes -----------------------------------------------------------

fn authorized(cfg: &Config, req: &Request) -> bool {
    if cfg.api_key.is_empty() {
        return true;
    }
    // the fleet TLS proxy strips Authorization: X-Api-Key or ?key= only
    if req.header("x-api-key") == Some(cfg.api_key.as_str()) {
        return true;
    }
    req.query.split('&').any(|kv| {
        kv.split_once('=')
            .map(|(k, v)| k == "key" && httpd::url_decode(v).as_deref() == Some(cfg.api_key.as_str()))
            .unwrap_or(false)
    })
}

/// Does this name (k51…/b…/12D3Koo…) refer to our identity?
fn is_our_name(app: &App, name: &str) -> bool {
    let Some(id) = &app.cfg.identity else { return false };
    multiformats::peer_id_str_decode(name).is_some_and(|mh| mh == id.peer_mh)
}

fn serve_record(app: &App) -> Response {
    let Some(rec) = &app.record else {
        return json(503, "Service Unavailable", "{\"error\":\"no record signed yet\"}".into());
    };
    let remaining = (app.validity_unix - now_unix()).max(0) as u64;
    let max_age = app.cfg.ttl_secs.min(remaining);
    Response::new(200, "OK")
        .with("cache-control", &format!("public, max-age={max_age}"))
        .with("x-ipns-sequence", &app.sequence.to_string())
        .body("application/vnd.ipfs.ipns-record", rec.clone())
}

fn route(app: &mut App, srv: &mut Server, key: usize, req: &Request) {
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/healthz") => srv.respond(key, json(200, "OK", "{\"ok\":true}".into())),
        ("GET", "/") => srv.respond(key, status_page(app, srv.uptime_secs())),
        ("GET", "/api/status") => srv.respond(key, api_status(app, srv.uptime_secs())),
        ("POST", "/publish") => {
            if !authorized(&app.cfg, req) {
                return srv.respond(key, json(401, "Unauthorized", "{\"error\":\"api key required\"}".into()));
            }
            if app.cfg.identity.is_none() {
                return srv.respond(key, json(503, "Service Unavailable", "{\"error\":\"no ipnsKey configured\"}".into()));
            }
            app.enqueue_publish();
            srv.respond(key, json(202, "Accepted", "{\"status\":\"publishing\"}".into()));
        }
        ("GET", _) if path.starts_with("/routing/v1/ipns/") => {
            let name = &path["/routing/v1/ipns/".len()..];
            if is_our_name(app, name) {
                srv.respond(key, serve_record(app));
            } else {
                srv.respond(key, json(404, "Not Found", "{\"error\":\"this publisher serves exactly one name\"}".into()));
            }
        }
        ("GET", _) if path.starts_with("/ipns/") => {
            let name = &path["/ipns/".len()..];
            let wants_record = req.query.split('&').any(|kv| kv == "format=ipns-record")
                || req.header("accept").is_some_and(|a| a.contains("application/vnd.ipfs.ipns-record"));
            if !is_our_name(app, name) {
                srv.respond(key, json(404, "Not Found", "{\"error\":\"this publisher serves exactly one name\"}".into()));
            } else if wants_record {
                srv.respond(key, serve_record(app));
            } else {
                srv.respond(key, json(406, "Not Acceptable", "{\"error\":\"ask with ?format=ipns-record\"}".into()));
            }
        }
        _ => srv.respond(key, json(404, "Not Found", "{\"error\":\"no such route\"}".into())),
    }
}

fn api_status(app: &App, uptime: u64) -> Response {
    let id_block = match &app.cfg.identity {
        Some(id) => format!(
            "\"peerId\":\"{}\",\"name\":\"{}\"",
            id.peer_id(),
            id.ipns_name()
        ),
        None => format!(
            "\"error\":\"{}\"",
            json_escape(app.cfg.key_error.as_deref().unwrap_or("unconfigured"))
        ),
    };
    let delegates: Vec<String> = app
        .delegate_results
        .iter()
        .map(|r| {
            format!(
                "{{\"url\":\"{}\",\"outcome\":\"{}\",\"at\":{}}}",
                json_escape(&r.url),
                json_escape(&r.outcome),
                r.at
            )
        })
        .collect();
    let (dht_mode, dht_detail) = match &app.dht {
        Some(d) => ("dht", d.status_json()),
        None => ("http-only", "null".to_string()),
    };
    let recovered = match &app.recovered_seq {
        Some((seq, _, src)) => format!("{{\"sequence\":{seq},\"source\":\"{}\"}}", json_escape(src)),
        None => "null".into(),
    };
    json(
        200,
        "OK",
        format!(
            "{{{id_block},\"sequence\":{},\"value\":\"{}\",\"validUntil\":{},\"publishedAt\":{},\"recovered\":{recovered},\"delegates\":[{}],\"mode\":\"{dht_mode}\",\"dht\":{dht_detail},\"durableState\":{},\"queuedTasks\":{},\"uptimeSecs\":{uptime}}}",
            app.sequence,
            json_escape(&String::from_utf8_lossy(&app.value)),
            app.validity_unix,
            app.published_at.unwrap_or(0),
            delegates.join(","),
            app.durable_state,
            app.tasks.len(),
        ),
    )
}

fn status_page(app: &App, uptime: u64) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let identity = match &app.cfg.identity {
        Some(id) => format!(
            "<tr><th>IPNS name</th><td><code>{}</code></td></tr>\n<tr><th>Peer ID</th><td><code>{}</code></td></tr>",
            id.ipns_name(),
            id.peer_id()
        ),
        None => format!(
            "<tr><th>Identity</th><td class=err>unconfigured: {}</td></tr>",
            esc(app.cfg.key_error.as_deref().unwrap_or("no key"))
        ),
    };
    let record = if app.record.is_some() {
        format!(
            "<tr><th>Value</th><td><code>{}</code></td></tr>\n<tr><th>Sequence</th><td>{}</td></tr>\n<tr><th>EOL</th><td>{}</td></tr>\n<tr><th>Signed at</th><td>{}</td></tr>",
            esc(&String::from_utf8_lossy(&app.value)),
            app.sequence,
            ipns::rfc3339(app.validity_unix),
            app.published_at.map(ipns::rfc3339).unwrap_or_default(),
        )
    } else {
        "<tr><th>Record</th><td class=err>none signed yet</td></tr>".to_string()
    };
    let mut delegates = String::new();
    for r in &app.delegate_results {
        let class = if r.outcome == "ok" { "ok" } else { "err" };
        delegates.push_str(&format!(
            "<tr><th>{}</th><td class={class}>{} <span class=dim>({})</span></td></tr>",
            esc(&r.url),
            esc(&r.outcome),
            ipns::rfc3339(r.at)
        ));
    }
    let dht = match &app.dht {
        Some(d) => esc(&d.status_line()),
        None => "http-only mode (no DHT publish yet)".to_string(),
    };
    let body = format!(
        r#"<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>ipns-publisher</title>
<style>
 body{{font:14px/1.5 system-ui,sans-serif;background:#101418;color:#d6dde4;max-width:780px;margin:2rem auto;padding:0 1rem}}
 h1{{font-size:1.2rem}} code{{word-break:break-all}}
 table{{border-collapse:collapse;width:100%}} th,td{{text-align:left;padding:.35rem .6rem;border-bottom:1px solid #232b33;vertical-align:top}}
 th{{white-space:nowrap;color:#8a97a3;font-weight:500;width:9rem}}
 .ok{{color:#7bd88f}} .err{{color:#ff8a80}} .dim{{color:#5c6873}}
</style>
<h1>ipns-publisher</h1>
<table>
{identity}
{record}
{delegates}
<tr><th>DHT</th><td>{dht}</td></tr>
<tr><th>Durable state</th><td>{}</td></tr>
<tr><th>Uptime</th><td>{uptime}s</td></tr>
</table>
<p class=dim>GET /routing/v1/ipns/&lt;name&gt; serves the signed record; POST /publish re-publishes now.</p>
"#,
        if app.durable_state { "yes (/data)" } else { "no (sequence recovery is network-only)" },
    );
    Response::new(200, "OK")
        .with("cache-control", "no-store")
        .body("text/html; charset=utf-8", body)
}

// ---- main ------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("id") => {
            let id = Identity::parse(&args[2]).expect("parse key");
            println!("peer-id:   {}", id.peer_id());
            println!("ipns-name: {}", id.ipns_name());
            println!("routing:   {}", multiformats::hex(&id.routing_key()));
        }
        // dhtpublish <key> <value> [ttl-secs] — build a record and PUT it on
        // the public DHT, driving the engine to completion with live logging.
        Some("dhtpublish") => {
            let id = Identity::parse(&args[2]).expect("parse key");
            let value = args[3].as_bytes().to_vec();
            let ttl_secs: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(3600);
            let validity = ipns::rfc3339(now_unix() + 48 * 3600);
            let record = ipns::build_record(&id, &value, &validity, 1, ttl_secs * 1_000_000_000)
                .expect("build record");
            eprintln!("[dhtpublish] name {} ({} record bytes)", id.ipns_name(), record.len());
            let bootstrap: Vec<String> = DEFAULT_BOOTSTRAP.iter().map(|s| s.to_string()).collect();
            let mut dht = p2p::Dht::new(bootstrap, id.signing.clone());
            dht.publish(id.routing_key(), record);
            let start = Instant::now();
            loop {
                let busy = dht.drive();
                let line = dht.status_line();
                if line.starts_with("done") || line.starts_with("failed") {
                    println!("[dhtpublish] {line}");
                    break;
                }
                if start.elapsed() > Duration::from_secs(200) {
                    println!("[dhtpublish] gave up after 200s: {line}");
                    break;
                }
                if !busy {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        Some("mkrecord") => {
            let id = Identity::parse(&args[2]).expect("parse key");
            let validity = match args[4].strip_prefix('+') {
                Some(secs) => ipns::rfc3339(now_unix() + secs.parse::<i64>().expect("secs")),
                None => args[4].clone(),
            };
            let rec = ipns::build_record(
                &id,
                args[3].as_bytes(),
                &validity,
                args[5].parse().expect("sequence"),
                args[6].parse().expect("ttl ns"),
            )
            .expect("build");
            use std::io::Write;
            std::io::stdout().write_all(&rec).expect("write");
        }
        _ => serve(),
    }
}

fn serve() {
    println!("[ipns-publisher] {APP}");
    let cfg = load_config();
    match &cfg.identity {
        Some(id) => {
            eprintln!("[ipns-publisher] publishing as {} ({})", id.ipns_name(), id.peer_id());
            eprintln!(
                "[ipns-publisher] value: {} | lifetime {}s | republish {}s | ttl {}s",
                cfg.value_src, cfg.lifetime_secs, cfg.republish_secs, cfg.ttl_secs
            );
        }
        None => eprintln!(
            "[ipns-publisher] starting UNCONFIGURED: {} — set the deployment secrets and restart",
            cfg.key_error.as_deref().unwrap_or("no ipnsKey")
        ),
    }
    let mut app = App::new(cfg);
    let mut srv = Server::bind("ipns-publisher", 8000);
    loop {
        for (key, req) in srv.poll(MAX_BODY, &[], "") {
            route(&mut app, &mut srv, key, &req);
        }
        let worked = app.tick();
        let p2p_busy = match &mut app.dht {
            Some(dht) => dht.drive(),
            None => false,
        };
        let flushed = srv.flush();
        if !(worked || p2p_busy || flushed) {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
