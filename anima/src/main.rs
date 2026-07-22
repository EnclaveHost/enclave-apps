//! anima — run a real machine on the enclave's CPU, booted from OS images in
//! an S3 bucket, with its serial console bridged to your browser.
//!
//! Unlike golem (which ships QEMU-wasm to the browser and emulates in the
//! tab), anima emulates a full RISC-V machine **inside the enclave** — the
//! way QEMU installed on a server would. A vendored pure-Rust RISC-V system
//! emulator (takahirox/riscv-rust: RV64GC, Sv39 MMU, CLINT/PLIC/UART/virtio
//! block) is the "CPU"; it compiles to the same `wasm32-wasip2` target as the
//! rest of the fleet and steps under wasmtime in the TEE. The enclave pulls
//! the kernel + root filesystem from a configured S3 bucket over transparent
//! egress (SigV4 when credentials are set; unsigned for public buckets),
//! boots them, and streams the UART console to the browser over SSE; your
//! keystrokes POST back into the guest. Disk writes the guest makes can be
//! saved back to the bucket with a single PUT.
//!
//! This is a run-mode SERVICE app: `wasmtime run` + wasi:sockets, one live
//! process holding the machine in RAM, HTTP served on the loopback `http:`
//! port the enclave's TLS proxy forwards to (see network-test / the suite's
//! httpd.rs). The single thread interleaves CPU batches with HTTP polling.
//!
//! Routes:
//!   GET  /            console UI (self-contained HTML + embedded xterm)
//!   GET  /a/<asset>   embedded xterm.js / xterm.css
//!   GET  /status      JSON machine state (phase, image sizes, instret, MIPS)
//!   POST /start       {accessKeyId?,secretAccessKey?,sessionToken?,reset?}
//!                     fetch images from S3 (creds: body > config > unsigned)
//!                     and boot; reset:true re-fetches instead of using cache
//!   POST /input       raw bytes → the guest UART receive register
//!   GET  /console     Server-Sent Events: base64 console output, scrollback first
//!   POST /save        dump the (guest-modified) disk and PUT it to saveKey
//!   POST /stop        halt the machine and drop it from RAM
//!   GET  /ping        liveness

mod httpd;
mod s3;

use std::collections::VecDeque;
use std::time::Instant;

use httpd::{json, Request, Response, Server};
use riscv_emu_rust::terminal::Terminal;
use riscv_emu_rust::Emulator;
use s3::{Creds, Endpoint};

static INDEX_HTML: &str = include_str!("index.html");
static XTERM_JS: &str = include_str!("vendor/xterm.js");
static XTERM_CSS: &str = include_str!("vendor/xterm.css");

const DEFAULT_PORT: u16 = 8000; // fleet policy: http:8000, never 8080
const MAX_BODY: usize = 256 * 1024;
const TICK_BATCH: u64 = 400_000; // CPU instructions per event-loop turn
const IDLE_BATCH: u64 = 4_000; // batch while the guest is parked in WFI: keeps
                               // timers/devices ticking at ~1% of the busy rate
                               // so an idle machine stops burning the host CPU
const SCROLLBACK: usize = 256 * 1024; // console bytes retained for late joiners

// ---- config ---------------------------------------------------------------

struct Config {
    title: String,
    endpoint: String,
    region: String,
    bucket: String,
    kernel: String,
    fs: String,
    dtb: Option<String>,
    save_key: Option<String>,
    config_creds: Option<Creds>,
    autostart: bool,
    read_only: bool,
}

fn creds_from(v: &serde_json::Value) -> Option<Creds> {
    let ak = v.get("accessKeyId").and_then(|x| x.as_str())?;
    let sk = v.get("secretAccessKey").and_then(|x| x.as_str())?;
    if ak.is_empty() || sk.is_empty() {
        return None;
    }
    Some(Creds {
        access_key_id: ak.to_string(),
        secret_access_key: sk.to_string(),
        session_token: v
            .get("sessionToken")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

fn load_config() -> Result<Config, String> {
    let raw = std::env::var("ENCLAVE_CONFIG")
        .or_else(|_| std::env::var("ANIMA_CONFIG"))
        .map_err(|_| "no ENCLAVE_CONFIG/ANIMA_CONFIG set".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("config is not JSON: {e}"))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let need = |k: &str| s(k).ok_or_else(|| format!("config missing \"{k}\""));
    Ok(Config {
        title: s("title").unwrap_or_else(|| "anima machine".to_string()),
        endpoint: need("endpoint")?,
        region: s("region").unwrap_or_else(|| "us-east-1".to_string()),
        bucket: need("bucket")?,
        kernel: need("kernel")?,
        fs: need("fs")?,
        dtb: s("dtb"),
        save_key: s("saveKey").or_else(|| s("fs")),
        config_creds: v.get("credentials").and_then(creds_from),
        autostart: v.get("autostart").and_then(|x| x.as_bool()).unwrap_or(false),
        read_only: v.get("readOnly").and_then(|x| x.as_bool()).unwrap_or(false),
    })
}

// ---- terminal: O(1) queues between the guest UART and HTTP -----------------

struct AnimaTerminal {
    input: VecDeque<u8>,
    output: VecDeque<u8>,
}
impl Terminal for AnimaTerminal {
    fn put_byte(&mut self, v: u8) {
        self.output.push_back(v);
    }
    fn get_output(&mut self) -> u8 {
        self.output.pop_front().unwrap_or(0)
    }
    fn put_input(&mut self, v: u8) {
        self.input.push_back(v);
    }
    fn get_input(&mut self) -> u8 {
        self.input.pop_front().unwrap_or(0)
    }
}

// ---- app state -------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Idle,
    Running,
    Halted,
    Error,
}

struct Images {
    kernel: Vec<u8>,
    fs: Vec<u8>,
    dtb: Option<Vec<u8>>,
}

struct Start {
    creds: Option<Creds>,
    reset: bool,
}

struct App {
    cfg: Config,
    emu: Option<Emulator>,
    phase: Phase,
    error: Option<String>,
    pending: Option<Start>,
    cache: Option<Images>,
    live_creds: Option<Creds>, // remembered from the last successful start, for /save
    instret: u64,
    boot_at: Option<Instant>,
    input_boost: u64, // turns to force full tick batches after POST /input
    scrollback: VecDeque<u8>,
    console_total: u64,
    last_save: Option<String>,
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

impl App {
    fn mips(&self) -> f64 {
        match self.boot_at {
            Some(t) if self.instret > 0 => {
                let s = t.elapsed().as_secs_f64();
                if s > 0.0 { self.instret as f64 / 1e6 / s } else { 0.0 }
            }
            _ => 0.0,
        }
    }

    fn status_json(&self) -> String {
        let phase = match self.phase {
            Phase::Idle => "idle",
            Phase::Running => "running",
            Phase::Halted => "halted",
            Phase::Error => "error",
        };
        let img = self
            .cache
            .as_ref()
            .map(|i| {
                format!(
                    ",\"kernelBytes\":{},\"fsBytes\":{}",
                    i.kernel.len(),
                    i.fs.len()
                )
            })
            .unwrap_or_default();
        format!(
            "{{\"phase\":\"{phase}\",\"title\":\"{}\",\"endpoint\":\"{}\",\"bucket\":\"{}\",\
             \"kernel\":\"{}\",\"fs\":\"{}\",\"saveKey\":{},\"readOnly\":{},\
             \"instret\":{},\"mips\":{:.1},\"consoleBytes\":{},\"lastSave\":{},\"error\":{}{img}}}",
            httpd::json_escape(&self.cfg.title),
            httpd::json_escape(&self.cfg.endpoint),
            httpd::json_escape(&self.cfg.bucket),
            httpd::json_escape(&self.cfg.kernel),
            httpd::json_escape(&self.cfg.fs),
            self.cfg
                .save_key
                .as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into()),
            self.cfg.read_only,
            self.instret,
            self.mips(),
            self.console_total,
            self.last_save
                .as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into()),
            self.error
                .as_ref()
                .map(|s| format!("\"{}\"", httpd::json_escape(s)))
                .unwrap_or_else(|| "null".into()),
        )
    }
}

// ---- image fetch + boot ----------------------------------------------------

fn fetch_images(cfg: &Config, creds: Option<&Creds>) -> Result<Images, String> {
    let ep = Endpoint::parse(&cfg.endpoint, &cfg.region)?;
    let mut noop = |_: usize, _: usize| {};
    eprintln!("[anima] fetching s3://{}/{}", cfg.bucket, cfg.kernel);
    let kernel = s3::get_object(&ep, &cfg.bucket, &cfg.kernel, creds, &mut noop)
        .map_err(|e| format!("fetch kernel {}: {e}", cfg.kernel))?;
    eprintln!("[anima]   kernel {} bytes; fetching {}", kernel.len(), cfg.fs);
    let fs = s3::get_object(&ep, &cfg.bucket, &cfg.fs, creds, &mut noop)
        .map_err(|e| format!("fetch fs {}: {e}", cfg.fs))?;
    eprintln!("[anima]   fs {} bytes", fs.len());
    let dtb = match &cfg.dtb {
        Some(k) => Some(
            s3::get_object(&ep, &cfg.bucket, k, creds, &mut noop)
                .map_err(|e| format!("fetch dtb {k}: {e}"))?,
        ),
        None => None,
    };
    Ok(Images { kernel, fs, dtb })
}

fn boot(images: &Images) -> Emulator {
    let mut emu = Emulator::new(Box::new(AnimaTerminal {
        input: VecDeque::new(),
        output: VecDeque::new(),
    }));
    emu.setup_program(images.kernel.clone());
    emu.setup_filesystem(images.fs.clone());
    if let Some(dtb) = &images.dtb {
        emu.setup_dtb(dtb.clone());
    }
    emu
}

// ---- request routing -------------------------------------------------------

fn route(app: &mut App, server: &mut Server, key: usize, req: Request) {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => server.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "no-store")
                .body("text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()),
        ),
        ("GET", "/ping") => server.respond(key, json(200, "OK", "{\"ok\":true}".into())),
        ("GET", "/a/xterm.js") => server.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "public, max-age=31536000, immutable")
                .body("text/javascript; charset=utf-8", XTERM_JS.as_bytes().to_vec()),
        ),
        ("GET", "/a/xterm.css") => server.respond(
            key,
            Response::new(200, "OK")
                .with("cache-control", "public, max-age=31536000, immutable")
                .body("text/css; charset=utf-8", XTERM_CSS.as_bytes().to_vec()),
        ),
        ("GET", "/status") => server.respond(key, json(200, "OK", app.status_json())),
        ("GET", "/console") => {
            // hand the late joiner the retained scrollback as the first frame
            let sb: Vec<u8> = app.scrollback.iter().copied().collect();
            let initial = if sb.is_empty() {
                String::new()
            } else {
                format!("data: {}\n\n", b64(&sb))
            };
            server.upgrade_sse(key, "console", &initial);
        }
        ("POST", "/start") => {
            if app.phase == Phase::Running {
                return server.respond(key, json(409, "Conflict", err("already running")));
            }
            let v: serde_json::Value =
                serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
            let creds = creds_from(&v);
            let reset = v.get("reset").and_then(|x| x.as_bool()).unwrap_or(false);
            app.pending = Some(Start { creds, reset });
            server.respond(key, json(202, "Accepted", "{\"ok\":true,\"phase\":\"loading\"}".into()));
        }
        ("POST", "/input") => {
            if let (Phase::Running, Some(emu)) = (app.phase, app.emu.as_mut()) {
                let t = emu.get_mut_terminal();
                for &b in &req.body {
                    t.put_input(b);
                }
                // run full batches until the UART has had time to drain this
                // input (it polls its terminal every ~230k ticks, one byte per
                // poll), else the idle throttle would add ~100ms per keystroke
                app.input_boost = app.input_boost.max(req.body.len() as u64 + 2);
                server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
            } else {
                server.respond(key, json(409, "Conflict", err("machine is not running")));
            }
        }
        ("POST", "/save") => save(app, server, key),
        ("POST", "/stop") => {
            app.emu = None;
            app.phase = Phase::Halted;
            app.boot_at = None;
            server.respond(key, json(200, "OK", "{\"ok\":true}".into()));
        }
        _ => server.respond(key, json(404, "Not Found", err("no such route"))),
    }
}

fn err(msg: &str) -> String {
    format!("{{\"error\":{{\"message\":\"{}\"}}}}", httpd::json_escape(msg))
}

fn save(app: &mut App, server: &mut Server, key: usize) {
    if app.cfg.read_only {
        return server.respond(key, json(403, "Forbidden", err("this machine is read-only")));
    }
    let Some(save_key) = app.cfg.save_key.clone() else {
        return server.respond(key, json(400, "Bad Request", err("no saveKey configured")));
    };
    let Some(emu) = app.emu.as_mut() else {
        return server.respond(key, json(409, "Conflict", err("machine is not running")));
    };
    let disk = emu.get_mut_cpu().get_mut_mmu().get_disk().dump_contents();
    let ep = match Endpoint::parse(&app.cfg.endpoint, &app.cfg.region) {
        Ok(e) => e,
        Err(e) => return server.respond(key, json(500, "Error", err(&e))),
    };
    // flush the 202-less response path: PUT blocks the loop, like /start's fetch
    eprintln!("[anima] saving {} bytes to s3://{}/{}", disk.len(), app.cfg.bucket, save_key);
    match s3::put_object(&ep, &app.cfg.bucket, &save_key, app.live_creds.as_ref(), &disk) {
        Ok(()) => {
            app.last_save = Some(save_key.clone());
            server.respond(
                key,
                json(200, "OK", format!("{{\"ok\":true,\"saved\":\"{}\",\"bytes\":{}}}",
                    httpd::json_escape(&save_key), disk.len())),
            )
        }
        Err(e) => server.respond(key, json(502, "Bad Gateway", err(&e))),
    }
}

/// Perform a queued /start: fetch (or reuse cached) images and boot.
fn do_start(app: &mut App, start: Start) {
    let need_fetch = start.reset || app.cache.is_none();
    if need_fetch {
        // creds precedence: request body > config; borrow-safe clone of config creds
        let body = start.creds;
        let chosen = body.as_ref().or(app.cfg.config_creds.as_ref());
        // stash which creds we used so /save can reuse them
        match fetch_images(&app.cfg, chosen) {
            Ok(imgs) => {
                app.live_creds = match &body {
                    Some(c) => Some(clone_creds(c)),
                    None => app.cfg.config_creds.as_ref().map(clone_creds),
                };
                app.cache = Some(imgs);
            }
            Err(e) => {
                eprintln!("[anima] start failed: {e}");
                app.error = Some(e);
                app.phase = Phase::Error;
                return;
            }
        }
    }
    let imgs = app.cache.as_ref().expect("cache present after fetch");
    app.emu = Some(boot(imgs));
    app.instret = 0;
    app.boot_at = Some(Instant::now());
    app.scrollback.clear();
    app.console_total = 0;
    app.error = None;
    app.phase = Phase::Running;
    eprintln!("[anima] machine running: {}", app.cfg.title);
}

fn clone_creds(c: &Creds) -> Creds {
    Creds {
        access_key_id: c.access_key_id.clone(),
        secret_access_key: c.secret_access_key.clone(),
        session_token: c.session_token.clone(),
    }
}

fn main() {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[anima] config error: {e}");
            eprintln!("[anima] set ENCLAVE_CONFIG (or ANIMA_CONFIG) to a JSON object with at least endpoint/bucket/kernel/fs");
            std::process::exit(1);
        }
    };
    let autostart = cfg.autostart;
    let mut server = Server::bind("anima", DEFAULT_PORT);
    let mut app = App {
        cfg,
        emu: None,
        phase: Phase::Idle,
        error: None,
        pending: if autostart { Some(Start { creds: None, reset: false }) } else { None },
        cache: None,
        live_creds: None,
        instret: 0,
        boot_at: None,
        input_boost: 0,
        scrollback: VecDeque::new(),
        console_total: 0,
        last_save: None,
    };

    loop {
        for (key, req) in server.poll(MAX_BODY) {
            route(&mut app, &mut server, key, req);
        }

        // Get responses (the 202 for /start, errors, etc.) onto the wire
        // BEFORE any blocking S3 work in do_start, so the browser isn't left
        // hanging on the fetch.
        server.flush();

        if let Some(start) = app.pending.take() {
            app.phase = Phase::Running; // optimistic; do_start flips to Error on failure
            do_start(&mut app, start);
        }

        let mut busy = false;
        if app.phase == Phase::Running {
            if let Some(emu) = app.emu.as_mut() {
                let batch = match app.input_boost == 0 && emu.get_cpu().is_idle() {
                    true => IDLE_BATCH,
                    false => TICK_BATCH,
                };
                app.input_boost = app.input_boost.saturating_sub(1);
                for _ in 0..batch {
                    emu.tick();
                }
                app.instret += batch;
                // drain the guest UART output into scrollback + SSE
                let mut chunk: Vec<u8> = Vec::new();
                let t = emu.get_mut_terminal();
                loop {
                    let b = t.get_output();
                    if b == 0 {
                        break;
                    }
                    chunk.push(b);
                    if chunk.len() >= 64 * 1024 {
                        break; // bound one drain; more comes next turn
                    }
                }
                if !chunk.is_empty() {
                    app.console_total += chunk.len() as u64;
                    for &b in &chunk {
                        if app.scrollback.len() >= SCROLLBACK {
                            app.scrollback.pop_front();
                        }
                        app.scrollback.push_back(b);
                    }
                    server.broadcast("console", &format!("data: {}", b64(&chunk)));
                    busy = true;
                }
            }
        }

        let flushed = server.flush();
        // Running with real CPU work paces the loop; only sleep when idle or
        // when a running machine produced no output and moved no bytes.
        if app.phase != Phase::Running {
            std::thread::sleep(std::time::Duration::from_millis(20));
        } else if !busy && !flushed {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
